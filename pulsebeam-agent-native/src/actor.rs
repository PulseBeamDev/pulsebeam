use tokio::task::JoinHandle;

use crate::agent::{AgentError, AgentRunner};

pub struct AgentActor;

impl AgentActor {
    pub fn spawn(runner: AgentRunner) -> JoinHandle<Result<(), AgentError>> {
        tokio::spawn(runner.run())
    }
}
