use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use crate::{
    MODEL_META_KEY,
    model::{self, Selection},
};
use agent_client_protocol::{
    Client, Error, Result,
    v2::{ConnectionTo, Responder, schema::*},
};
use elphie_core::{Completion, FinishReason, Message, ModelOption, Provider, ProviderError, Role};
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

enum Outcome {
    Reply(Completion),
    Models(Vec<ModelOption>),
    Selection(Selection),
    FormUnsupported,
}

type Pending = Pin<Box<dyn Future<Output = Result<Outcome>> + Send>>;

enum Work {
    Running(Pending),
    Choosing(Pending),
}

impl Work {
    async fn wait(&mut self) -> Result<Outcome> {
        match self {
            Work::Running(pending) | Work::Choosing(pending) => pending.await,
        }
    }
}

pub(super) struct Session {
    id: SessionId,
    cwd: AbsolutePath,
    provider: Arc<dyn Provider>,
    client: ConnectionTo<Client>,
    active: bool,
    supports_form: bool,
    model: String,
    transcript: Vec<SessionUpdate>,
    messages: Vec<Message>,
    inserted: HashMap<IdempotencyKey, MessageId>,
    held: VecDeque<Submission>,
    work: Option<Work>,
}

impl Session {
    pub(super) fn new(
        id: SessionId,
        cwd: AbsolutePath,
        provider: Arc<dyn Provider>,
        client: ConnectionTo<Client>,
        supports_form: bool,
    ) -> Self {
        Self {
            id,
            cwd,
            model: provider.default_model().to_owned(),
            provider,
            client,
            active: true,
            supports_form,
            transcript: Vec::new(),
            messages: Vec::new(),
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
                outcome = async { self.work.as_mut().unwrap().wait().await }, if self.work.is_some() => {
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
                    for update in &self.transcript {
                        self.send(update.clone())?;
                    }
                    self.send(SessionUpdate::ReplayComplete(ReplayComplete::new()))?;
                }
                self.send(SessionUpdate::SessionInfoUpdate(
                    SessionInfoUpdate::new().title(None).updated_at(None),
                ))?;
                self.send_state(None)?;
                response.respond(
                    ResumeSessionResponse::new()
                        .available_commands(model::commands(self.supports_form)),
                )
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
        let model_command = matches!(submission.content.as_slice(), [ContentBlock::Text(t)] if t.text.trim_end() == "/model");
        let user =
            SessionUpdate::UserMessage(UserMessage::new(id.clone()).content(submission.content));
        if let Some(key) = submission.key {
            self.inserted.insert(key, id.clone());
        }
        for response in submission.responses {
            response.respond(PromptResponse::new(id.clone()))?;
        }
        self.record(user)?;
        let provider = self.provider.clone();
        let pending: Pending = if model_command {
            let supports_form = self.supports_form;
            Box::pin(async move {
                if !supports_form {
                    return Ok(Outcome::FormUnsupported);
                }
                provider
                    .models()
                    .await
                    .map(Outcome::Models)
                    .map_err(provider_error)
            })
        } else {
            self.messages.push(Message {
                role: Role::User,
                text: submission.text,
            });
            let messages = self.messages.clone();
            let model = self.model.clone();
            Box::pin(async move {
                provider
                    .complete(&model, &messages)
                    .await
                    .map(Outcome::Reply)
                    .map_err(provider_error)
            })
        };
        self.work = Some(Work::Running(pending));
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

    fn finish(&mut self, outcome: Result<Outcome>) -> Result<()> {
        let reason = match outcome {
            Ok(Outcome::Reply(Completion { text, reason })) => {
                if !text.is_empty() {
                    self.messages.push(Message {
                        role: Role::Assistant,
                        text: text.clone(),
                    });
                    self.agent_message(text, None)?;
                }
                match reason {
                    FinishReason::EndTurn => StopReason::EndTurn,
                    FinishReason::MaxTokens => StopReason::MaxTokens,
                    FinishReason::Refusal => StopReason::Refusal,
                }
            }
            Ok(Outcome::FormUnsupported) => {
                self.agent_message(
                    "Use a client with form support to select a model.".into(),
                    None,
                )?;
                StopReason::EndTurn
            }
            Ok(Outcome::Models(models)) => {
                match model::request(self.id.clone(), &self.model, &models) {
                    Ok(request) => {
                        let selection = model::select(self.client.clone(), request, models);
                        self.work = Some(Work::Choosing(Box::pin(async move {
                            selection.await.map(Outcome::Selection)
                        })));
                        return self.send_state(None);
                    }
                    Err(error) => error_stop(error),
                }
            }
            Ok(Outcome::Selection(Selection::Selected(model))) => {
                self.send_state_update(StateUpdate::Running(
                    RunningStateUpdate::new().background(vec![]),
                ))?;
                self.model = model.id;
                let mut meta = Meta::new();
                meta.insert(MODEL_META_KEY.into(), self.model.clone().into());
                self.agent_message(format!("Model set to {}.", model.name), Some(meta))?;
                StopReason::EndTurn
            }
            Ok(Outcome::Selection(Selection::Declined)) => StopReason::EndTurn,
            Ok(Outcome::Selection(Selection::Cancelled)) => StopReason::Cancelled,
            Err(error) => error_stop(error),
        };
        self.send_state(Some(reason))
    }

    fn agent_message(&mut self, text: String, meta: Option<Meta>) -> Result<()> {
        let id = MessageId::new(Uuid::new_v4().to_string());
        self.record(SessionUpdate::AgentMessage(
            AgentMessage::new(id).content(vec![text.into()]).meta(meta),
        ))
    }

    fn record(&mut self, update: SessionUpdate) -> Result<()> {
        self.transcript.push(update.clone());
        self.send(update)
    }

    fn send_state(&self, reason: Option<StopReason>) -> Result<()> {
        let state = match self.work {
            Some(Work::Running(_)) => {
                StateUpdate::Running(RunningStateUpdate::new().background(vec![]))
            }
            Some(Work::Choosing(_)) => {
                StateUpdate::RequiresAction(RequiresActionStateUpdate::new().background(vec![]))
            }
            None => StateUpdate::Idle(
                IdleStateUpdate::new()
                    .stop_reason(reason)
                    .background(vec![]),
            ),
        };
        self.send_state_update(state)
    }

    fn send_state_update(&self, state: StateUpdate) -> Result<()> {
        self.send(SessionUpdate::StateUpdate(state))
    }

    fn send(&self, update: SessionUpdate) -> Result<()> {
        self.client
            .send_notification(UpdateSessionNotification::new(self.id.clone(), update))
    }
}

fn provider_error(error: ProviderError) -> Error {
    match error {
        ProviderError::AuthenticationRequired(message) => {
            Error::new(ErrorCode::AuthRequired, message)
        }
        ProviderError::Failed(message) => Error::new(ErrorCode::InternalError, message),
    }
}

fn error_stop(error: Error) -> StopReason {
    StopReason::Error(
        ErrorStopReason::new().error(agent_client_protocol::v2::schema::Error::from(error)),
    )
}
