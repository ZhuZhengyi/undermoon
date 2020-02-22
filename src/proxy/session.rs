use super::backend::{CmdTask, CmdTaskFactory, TaskResult};
use super::command::{
    new_command_pair, CmdReplySender, CmdType, Command, CommandError, CommandResult, DataCmdType,
    TaskReply,
};
use super::database::{DBTag, DEFAULT_DB};
use super::slowlog::{SlowRequestLogger, Slowlog, TaskEvent};
use crate::common::batch::TryChunksTimeoutStreamExt;
use crate::common::cluster::DBName;
use crate::protocol::{
    new_simple_packet_codec, DecodeError, EncodeError, Resp, RespCodec, RespPacket, RespVec,
};
use futures::{stream, Future, TryFutureExt};
use futures::{SinkExt, StreamExt, TryStreamExt};
use std::boxed::Box;
use std::error::Error;
use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_util::codec::Decoder;

// TODO: Let it return future to support multi-key commands.
pub trait CmdHandler {
    fn handle_cmd(&self, cmd: Command, sender: CmdReplySender);
    fn handle_slowlog(&self, request: Box<RespPacket>, slowlog: Slowlog);
}

pub trait CmdCtxHandler {
    fn handle_cmd_ctx(&self, cmd_ctx: CmdCtx);
}

#[derive(Debug)]
pub struct CmdCtx {
    db: sync::Arc<sync::RwLock<DBName>>,
    cmd: Command,
    reply_sender: CmdReplySender,
    slowlog: Slowlog,
}

impl CmdCtx {
    pub fn new(
        db: sync::Arc<sync::RwLock<DBName>>,
        cmd: Command,
        reply_sender: CmdReplySender,
        session_id: usize,
    ) -> CmdCtx {
        let slowlog = Slowlog::new(session_id);
        CmdCtx {
            db,
            cmd,
            reply_sender,
            slowlog,
        }
    }

    pub fn get_cmd(&self) -> &Command {
        &self.cmd
    }

    pub fn get_db(&self) -> sync::Arc<sync::RwLock<DBName>> {
        self.db.clone()
    }

    pub fn get_session_id(&self) -> usize {
        self.slowlog.get_session_id()
    }

    pub fn change_cmd_element(&mut self, index: usize, data: Vec<u8>) -> bool {
        self.cmd.change_element(index, data)
    }

    pub fn get_cmd_type(&self) -> CmdType {
        self.cmd.get_type()
    }

    pub fn get_data_cmd_type(&self) -> DataCmdType {
        self.cmd.get_data_cmd_type()
    }
}

impl CmdTask for CmdCtx {
    type Pkt = RespPacket;

    fn get_key(&self) -> Option<&[u8]> {
        self.get_cmd().get_key()
    }

    fn set_result(self, result: CommandResult<Self::Pkt>) {
        let Self {
            cmd,
            mut reply_sender,
            slowlog,
            ..
        } = self;
        let task_result =
            result.map(|packet| Box::new(TaskReply::new(cmd.into_packet(), packet, slowlog)));
        let res = reply_sender.send(task_result);
        if let Err(e) = res {
            error!("Failed to send result: {:?}", e);
        }
    }

    fn get_packet(&self) -> Self::Pkt {
        self.cmd.get_packet()
    }

    fn set_resp_result(self, result: Result<RespVec, CommandError>)
    where
        Self: Sized,
    {
        self.set_result(result.map(|resp| Box::new(RespPacket::from(resp))))
    }

    fn log_event(&self, event: TaskEvent) {
        self.slowlog.log_event(event);
    }
}

impl DBTag for CmdCtx {
    fn get_db_name(&self) -> DBName {
        self.db.read().expect("CmdCtx::new").clone()
    }

    fn set_db_name(&mut self, db: DBName) {
        *self.db.write().expect("CmdCtx::set_db_name") = db
    }
}

pub struct CmdCtxFactory;

impl Default for CmdCtxFactory {
    fn default() -> Self {
        Self
    }
}

impl CmdTaskFactory for CmdCtxFactory {
    type Task = CmdCtx;

    fn create_with(
        &self,
        another_task: &Self::Task,
        resp: RespVec,
    ) -> (
        Self::Task,
        Pin<Box<dyn Future<Output = TaskResult> + Send + 'static>>,
    ) {
        let packet = Box::new(RespPacket::from_resp_vec(resp));
        let cmd = Command::new(packet);
        let (reply_sender, reply_receiver) = new_command_pair();
        let cmd_ctx = CmdCtx::new(
            another_task.get_db(),
            cmd,
            reply_sender,
            another_task.get_session_id(),
        );
        let fut = reply_receiver
            .wait_response()
            .map_ok(|reply| reply.into_resp_vec());
        (cmd_ctx, Box::pin(fut))
    }
}

pub struct Session<H: CmdCtxHandler> {
    session_id: usize,
    db: sync::Arc<sync::RwLock<DBName>>,
    cmd_ctx_handler: H,
    slow_request_logger: sync::Arc<SlowRequestLogger>,
}

impl<H: CmdCtxHandler> Session<H> {
    pub fn new(
        session_id: usize,
        cmd_ctx_handler: H,
        slow_request_logger: sync::Arc<SlowRequestLogger>,
    ) -> Self {
        let dbname = DBName::from(DEFAULT_DB).expect("Session::new");
        Session {
            session_id,
            db: sync::Arc::new(sync::RwLock::new(dbname)),
            cmd_ctx_handler,
            slow_request_logger,
        }
    }
}

impl<H: CmdCtxHandler> CmdHandler for Session<H> {
    fn handle_cmd(&self, cmd: Command, reply_sender: CmdReplySender) {
        let cmd_ctx = CmdCtx::new(self.db.clone(), cmd, reply_sender, self.session_id);
        cmd_ctx.log_event(TaskEvent::Created);
        self.cmd_ctx_handler.handle_cmd_ctx(cmd_ctx);
    }

    fn handle_slowlog(&self, request: Box<RespPacket>, slowlog: Slowlog) {
        self.slow_request_logger.add_slow_log(request, slowlog)
    }
}

pub async fn handle_session<H>(
    handler: sync::Arc<H>,
    sock: TcpStream,
    _channel_size: usize,
    session_batch_min_time: usize,
    session_batch_max_time: usize,
    session_batch_buf: NonZeroUsize,
) -> Result<(), SessionError>
where
    H: CmdHandler + Send + Sync + 'static,
{
    let (encoder, decoder) = new_simple_packet_codec::<Box<RespPacket>, Box<RespPacket>>();
    let (mut writer, reader) = RespCodec::new(encoder, decoder).framed(sock).split();
    let mut reader = reader
        .map_err(|e| match e {
            DecodeError::Io(e) => SessionError::Io(e),
            DecodeError::InvalidProtocol => SessionError::Canceled,
        })
        .try_chunks_timeout(
            session_batch_buf,
            Duration::from_nanos(session_batch_min_time as u64),
            Duration::from_nanos(session_batch_max_time as u64),
        );

    let mut reply_receiver_list = Vec::with_capacity(session_batch_buf.get());
    let mut replies = Vec::with_capacity(session_batch_buf.get());
    let output = reader.get_output_buf();

    while let Some(()) = reader.next().await {
        {
            let mut reqs = match output.lock() {
                Ok(reqs) => reqs,
                Err(_) => return Err(SessionError::InvalidState),
            };

            for req in reqs.drain(..) {
                let packet = match req {
                    Ok(packet) => packet,
                    Err(err) => {
                        error!("session reader error {:?}", err);
                        return Err(err);
                    }
                };
                let cmd = Command::new(packet);
                let (reply_sender, reply_receiver) = new_command_pair();

                handler.handle_cmd(cmd, reply_sender);
                reply_receiver_list.push(reply_receiver);
            }
        }

        for reply_receiver in reply_receiver_list.drain(..) {
            let res = reply_receiver
                .wait_response()
                .await
                .map_err(SessionError::CmdErr);
            let packet = match res {
                Ok(task_reply) => {
                    let (request, packet, slowlog) = (*task_reply).into_inner();
                    slowlog.log_event(TaskEvent::WaitDone);
                    handler.handle_slowlog(request, slowlog);
                    packet
                }
                Err(e) => {
                    let err_msg = format!("Err cmd error {:?}", e);
                    error!("{}", err_msg);
                    let resp = Resp::Error(err_msg.into_bytes());
                    Box::new(RespPacket::from_resp_vec(resp))
                }
            };

            replies.push(packet);
        }

        let mut batch = stream::iter(replies.drain(..)).map(Ok);
        if let Err(err) = writer.send_all(&mut batch).await {
            error!("writer error: {}", err);
            let err = match err {
                EncodeError::Io(err) => SessionError::Io(err),
                EncodeError::NotReady(_) => SessionError::InvalidState,
            };
            return Err(err);
        }
    }

    Ok(())
}

#[derive(Debug)]
pub enum SessionError {
    Io(io::Error),
    CmdErr(CommandError),
    InvalidProtocol,
    Canceled,
    InvalidState,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for SessionError {
    fn description(&self) -> &str {
        "session error"
    }

    fn cause(&self) -> Option<&dyn Error> {
        match self {
            SessionError::Io(err) => Some(err),
            SessionError::CmdErr(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Array, BulkStr, Resp};
    use matches::assert_matches;
    use std::sync::{Arc, RwLock};
    use tokio;

    #[tokio::test]
    async fn test_cmd_ctx_auto_send() {
        let request = RespPacket::Data(Resp::Arr(Array::Arr(vec![Resp::Bulk(BulkStr::Str(
            b"PING".to_vec(),
        ))])));
        let db = Arc::new(RwLock::new(DBName::from("mydb").unwrap()));
        let cmd = Command::new(Box::new(request));
        let (sender, receiver) = new_command_pair();
        let cmd_ctx = CmdCtx::new(db, cmd, sender, 7799);
        drop(cmd_ctx);
        let err = match receiver.wait_response().await {
            Ok(_) => panic!(),
            Err(err) => err,
        };
        assert_matches!(err, CommandError::Dropped);
    }
}
