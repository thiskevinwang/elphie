use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use agent_client_protocol::{
    Client, Error, Result,
    v2::{ConnectionTo, Responder, schema::*},
};
use elphie_core::{
    Completion, FinishReason, Message, Provider, ProviderError, ProviderResult, Role,
};
use tokio::sync::mpsc;
use uuid::Uuid;

pub(super) enum Command {
    Prompt(
        Vec<ContentBlock>,
        Option<IdempotencyKey>,
        Responder<PromptResponse>,
    ),
    Resume(
        AbsolutePath,
        Option<ReplayFrom>,
        Responder<ResumeSessionResponse>,
    ),
    Cancel,
    Close(Responder<CloseSessionResponse>),
}

impl Command {
    pub(super) fn unknown_session(self, id: &SessionId) -> Result<()> {
        let missing = agent_client_protocol::v2::schema::Error::session_not_found(id.clone());
        match self {
            Self::Prompt(_, _, response) => response.respond_with_error(missing.into()),
            Self::Resume(_, _, response) => response.respond_with_error(missing.into()),
            Self::Cancel => Ok(()),
            Self::Close(response) => response.respond(CloseSessionResponse::new()),
        }
    }
}

struct Submission {
    key: Option<IdempotencyKey>,
    text: String,
    content: Vec<ContentBlock>,
    responses: Vec<Responder<PromptResponse>>,
}

type PendingReply = Pin<Box<dyn Future<Output = ProviderResult> + Send>>;

pub(super) struct Session {
    id: SessionId,
    cwd: AbsolutePath,
    provider: Arc<dyn Provider>,
    client: ConnectionTo<Client>,
    active: bool,
    history: Vec<(Message, SessionUpdate)>,
    inserted: HashMap<IdempotencyKey, MessageId>,
    held: VecDeque<Submission>,
    work: Option<PendingReply>,
}

impl Session {
    pub(super) fn new(
        id: SessionId,
        cwd: AbsolutePath,
        provider: Arc<dyn Provider>,
        client: ConnectionTo<Client>,
    ) -> Self {
        Self {
            id,
            cwd,
            provider,
            client,
            active: true,
            history: Vec::new(),
            inserted: HashMap::new(),
            held: VecDeque::new(),
            work: None,
        }
    }

    pub(super) async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command)?,
                    None => return Ok(()),
                },
                outcome = async { self.work.as_mut().unwrap().await }, if self.work.is_some() => {
                    self.work = None;
                    self.finish(outcome)?;
                }
            }
            if self.work.is_none()
                && let Some(next) = self.held.pop_front()
            {
                self.insert(next)?;
            }
        }
    }

    fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Resume(cwd, cursor, response) => {
                if cwd != self.cwd
                    || cursor
                        .as_ref()
                        .is_some_and(|c| !matches!(c, ReplayFrom::Start(_)))
                {
                    return response.respond_with_error(Error::invalid_params());
                }
                self.active = true;
                if cursor.is_some() {
                    for (_, update) in &self.history {
                        self.send(update.clone())?;
                    }
                    self.send(SessionUpdate::ReplayComplete(ReplayComplete::new()))?;
                }
                self.send(SessionUpdate::SessionInfoUpdate(
                    SessionInfoUpdate::new().title(None).updated_at(None),
                ))?;
                self.send_state(None)?;
                response.respond(ResumeSessionResponse::new())
            }
            command if !self.active => command.unknown_session(&self.id),
            Command::Prompt(content, key, response) => self.enqueue(content, key, response),
            Command::Cancel => self.cancel(),
            Command::Close(response) => {
                self.cancel()?;
                self.active = false;
                response.respond(CloseSessionResponse::new())
            }
        }
    }

    fn enqueue(
        &mut self,
        content: Vec<ContentBlock>,
        key: Option<IdempotencyKey>,
        response: Responder<PromptResponse>,
    ) -> Result<()> {
        if content.is_empty() {
            return response.respond_with_error(Error::invalid_params());
        }
        let mut pieces = Vec::with_capacity(content.len());
        for block in &content {
            match block {
                ContentBlock::Text(t) => pieces.push(t.text.clone()),
                ContentBlock::ResourceLink(link) => {
                    pieces.push(format!("{} ({})", link.name, link.uri))
                }
                _ => return response.respond_with_error(Error::invalid_params()),
            }
        }
        if let Some(key) = &key {
            if let Some(id) = self.inserted.get(key) {
                return response.respond(PromptResponse::new(id.clone()));
            }
            if let Some(pending) = self.held.iter_mut().find(|p| p.key.as_ref() == Some(key)) {
                pending.responses.push(response);
                return Ok(());
            }
        }
        self.held.push_back(Submission {
            key,
            text: pieces.join("\n"),
            content,
            responses: vec![response],
        });
        Ok(())
    }

    fn insert(&mut self, submission: Submission) -> Result<()> {
        let id = MessageId::new(Uuid::new_v4().to_string());
        let user =
            SessionUpdate::UserMessage(UserMessage::new(id.clone()).content(submission.content));
        self.history.push((
            Message {
                role: Role::User,
                text: submission.text,
            },
            user.clone(),
        ));
        if let Some(key) = submission.key {
            self.inserted.insert(key, id.clone());
        }
        for response in submission.responses {
            response.respond(PromptResponse::new(id.clone()))?;
        }
        self.send(user)?;
        let messages: Vec<_> = self
            .history
            .iter()
            .map(|(message, _)| message.clone())
            .collect();
        let provider = self.provider.clone();
        self.work = Some(Box::pin(async move { provider.complete(&messages).await }));
        self.send_state(None)
    }

    fn cancel(&mut self) -> Result<()> {
        for held in self.held.drain(..) {
            for response in held.responses {
                response.respond_with_error(Error::request_cancelled())?;
            }
        }
        if self.work.take().is_some() {
            self.send_state(Some(StopReason::Cancelled))?;
        }
        Ok(())
    }

    fn finish(&mut self, outcome: ProviderResult) -> Result<()> {
        let reason = match outcome {
            Ok(Completion { text, reason }) => {
                if !text.is_empty() {
                    let id = MessageId::new(Uuid::new_v4().to_string());
                    let update = SessionUpdate::AgentMessage(
                        AgentMessage::new(id).content(vec![text.clone().into()]),
                    );
                    self.history.push((
                        Message {
                            role: Role::Assistant,
                            text,
                        },
                        update.clone(),
                    ));
                    self.send(update)?;
                }
                match reason {
                    FinishReason::EndTurn => StopReason::EndTurn,
                    FinishReason::MaxTokens => StopReason::MaxTokens,
                    FinishReason::Refusal => StopReason::Refusal,
                }
            }
            Err(error) => {
                let (code, message) = match error {
                    ProviderError::AuthenticationRequired(message) => {
                        (ErrorCode::AuthRequired, message)
                    }
                    ProviderError::Failed(message) => (ErrorCode::InternalError, message),
                };
                StopReason::Error(ErrorStopReason::new().error(
                    agent_client_protocol::v2::schema::Error::new(code.into(), message),
                ))
            }
        };
        self.send_state(Some(reason))
    }

    fn send_state(&self, reason: Option<StopReason>) -> Result<()> {
        let state = if self.work.is_some() {
            StateUpdate::Running(RunningStateUpdate::new().background(vec![]))
        } else {
            StateUpdate::Idle(
                IdleStateUpdate::new()
                    .stop_reason(reason)
                    .background(vec![]),
            )
        };
        self.send(SessionUpdate::StateUpdate(state))
    }

    fn send(&self, update: SessionUpdate) -> Result<()> {
        self.client
            .send_notification(UpdateSessionNotification::new(self.id.clone(), update))
    }
}
