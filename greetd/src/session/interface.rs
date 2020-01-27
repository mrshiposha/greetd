use std::{
    os::unix::{io::AsRawFd, net::UnixDatagram},
    process::Command,
};

use nix::{
    fcntl::{fcntl, FcntlArg, FdFlag},
    sys::signal::Signal,
    unistd::Pid,
};

use async_trait::async_trait;

use tokio::net::UnixDatagram as TokioUnixDatagram;

use super::worker::{ParentToSessionChild, QuestionStyle, SessionChildToParent};
use crate::error::Error;

#[async_trait]
trait AsyncRecv<T: Sized> {
    async fn recv(sock: &mut TokioUnixDatagram) -> Result<T, Error>;
}

#[async_trait]
trait AsyncSend {
    async fn send(&self, sock: &mut TokioUnixDatagram) -> Result<(), Error>;
}

#[async_trait]
impl AsyncSend for ParentToSessionChild {
    async fn send(&self, sock: &mut TokioUnixDatagram) -> Result<(), Error> {
        let out =
            serde_json::to_vec(self).map_err(|e| format!("unable to serialize message: {}", e))?;
        sock.send(&out)
            .await
            .map_err(|e| format!("unable to send message: {}", e))?;
        Ok(())
    }
}

#[async_trait]
impl AsyncRecv<SessionChildToParent> for SessionChildToParent {
    async fn recv(sock: &mut TokioUnixDatagram) -> Result<SessionChildToParent, Error> {
        let mut data = [0; 10240];
        let len = sock
            .recv(&mut data[..])
            .await
            .map_err(|e| format!("unable to recieve message: {}", e))?;
        let msg = serde_json::from_slice(&data[..len])
            .map_err(|e| format!("unable to deserialize message: {}", e))?;
        Ok(msg)
    }
}

/// SessionChild tracks the processes spawned by a session
pub struct SessionChild {
    task: Pid,
    sub_task: Pid,
}

impl SessionChild {
    /// Check if this session has this pid.
    pub fn owns_pid(&self, pid: Pid) -> bool {
        self.task == pid || self.sub_task == pid
    }

    /// Send SIGTERM to the session child.
    pub fn term(&self) {
        let _ = nix::sys::signal::kill(self.sub_task, Signal::SIGTERM);
    }

    /// Send SIGKILL to the session child.
    pub fn kill(&self) {
        let _ = nix::sys::signal::kill(self.sub_task, Signal::SIGKILL);
        let _ = nix::sys::signal::kill(self.task, Signal::SIGKILL);
    }
}

#[derive(Debug)]
pub enum SessionState {
    Question(QuestionStyle, String),
    Ready,
}

/// A device to initiate a logged in PAM session.
pub struct Session {
    task: Pid,
    sock: TokioUnixDatagram,
    last_msg: Option<SessionChildToParent>,
}

impl Session {
    /// Create a session started as an external process.
    pub fn new_external() -> Result<Session, Error> {
        // Pipe used to communicate the true PID of the final child.
        let (parentfd, childfd) =
            UnixDatagram::pair().map_err(|e| format!("could not create pipe: {}", e))?;

        let raw_child = childfd.as_raw_fd();
        let mut cur_flags =
            unsafe { FdFlag::from_bits_unchecked(fcntl(raw_child, FcntlArg::F_GETFD)?) };
        cur_flags.remove(FdFlag::FD_CLOEXEC);
        fcntl(raw_child, FcntlArg::F_SETFD(cur_flags))?;

        let child = Command::new(std::env::current_exe()?)
            .arg("--session-worker")
            .arg(format!("{}", raw_child as usize))
            .spawn()?;

        Ok(Session {
            task: Pid::from_raw(child.id() as i32),
            sock: TokioUnixDatagram::from_std(parentfd)?,
            last_msg: None,
        })
    }

    /// Initiates the session, which will cause authentication to begin.
    pub async fn initiate(
        &mut self,
        service: &str,
        class: &str,
        user: &str,
        authenticate: bool,
    ) -> Result<(), Error> {
        let msg = ParentToSessionChild::InitiateLogin {
            service: service.to_string(),
            class: class.to_string(),
            user: user.to_string(),
            authenticate,
        };
        msg.send(&mut self.sock).await?;
        Ok(())
    }

    /// Return the current state of this session.
    pub async fn get_state(&mut self) -> Result<SessionState, Error> {
        let msg = match self.last_msg.take() {
            Some(msg) => msg,
            None => SessionChildToParent::recv(&mut self.sock).await?,
        };

        self.last_msg = Some(msg.clone());

        match msg {
            SessionChildToParent::PamMessage { style, msg } => {
                Ok(SessionState::Question(style, msg))
            }
            SessionChildToParent::Success => Ok(SessionState::Ready),
            SessionChildToParent::Error(e) => Err(e),
            msg => panic!("unexpected message from session worker: {:?}", msg),
        }
    }

    /// Cancel the session.
    pub async fn cancel(&mut self) -> Result<(), Error> {
        self.last_msg = None;
        ParentToSessionChild::Cancel.send(&mut self.sock).await?;
        Ok(())
    }

    /// Send an answer to an authentication question, or None to cahncel the
    /// authentication attempt.
    pub async fn post_answer(&mut self, answer: Option<String>) -> Result<(), Error> {
        self.last_msg = None;
        let msg = match answer {
            Some(resp) => ParentToSessionChild::PamResponse { resp },
            None => ParentToSessionChild::Cancel,
        };
        msg.send(&mut self.sock).await?;
        Ok(())
    }

    ///
    /// Send the arguments that will be used to start the session.
    ///
    pub async fn send_args(
        &mut self,
        cmd: Vec<String>,
        env: Vec<String>,
        vt: usize,
    ) -> Result<(), Error> {
        let msg = ParentToSessionChild::Args { vt, env, cmd };
        msg.send(&mut self.sock).await?;

        let msg = SessionChildToParent::recv(&mut self.sock).await?;

        self.last_msg = Some(msg.clone());

        match msg {
            SessionChildToParent::Success => Ok(()),
            SessionChildToParent::Error(e) => Err(e),
            msg => panic!("unexpected message from session worker: {:?}", msg),
        }
    }

    ///
    /// Start the session.
    ///
    pub async fn start(&mut self) -> Result<SessionChild, Error> {
        let msg = ParentToSessionChild::Start;
        msg.send(&mut self.sock).await?;

        let msg = SessionChildToParent::recv(&mut self.sock).await?;

        self.sock.shutdown(std::net::Shutdown::Both)?;

        let sub_task = match msg {
            SessionChildToParent::Error(e) => return Err(e),
            SessionChildToParent::FinalChildPid(raw_pid) => Pid::from_raw(raw_pid as i32),
            msg => panic!("unexpected message from session worker: {:?}", msg),
        };

        Ok(SessionChild {
            task: self.task,
            sub_task,
        })
    }
}