use super::*;

fn ready() -> Chat {
    let mut chat = Chat::new("small");
    chat.apply(Event::Phase(Phase::Ready));
    chat
}

#[test]
fn tool_patches_update_one_row_and_keep_message_order() {
    let mut chat = ready();
    chat.apply(Event::ToolUpdate {
        id: "1".into(),
        title: Some("Read notes.txt".into()),
        status: Some(ToolStatus::InProgress),
    });
    chat.apply(Event::Message(
        Role::Assistant,
        "1".into(),
        Edit::Replace("Found it".into()),
    ));
    for _ in 0..2 {
        chat.apply(Event::ToolUpdate {
            id: "1".into(),
            title: None,
            status: Some(ToolStatus::Completed),
        });
    }
    assert!(
        matches!(chat.entries(), [TranscriptEntry::Tool(tool), TranscriptEntry::Message { .. }]
        if tool.status == ToolStatus::Completed && tool.title == "Read notes.txt")
    );
}

fn send(chat: &mut Chat, text: &str) {
    chat.handle(Intent::Paste(text.into()));
    assert_eq!(chat.handle(Intent::Submit), Some(Action::Send(text.into())));
    chat.apply(Event::Phase(Phase::Ready));
}

fn picker(request_id: FormId) -> Event {
    Event::ChooseModel {
        request_id,
        message: "Select a model".into(),
        current: Some("small".into()),
        models: [
            ("alpha-8b", "Sage"),
            ("small", "Daily"),
            ("nova", "Deep Reasoner"),
        ]
        .into_iter()
        .map(|(id, name)| ModelOption {
            id: id.into(),
            name: name.into(),
        })
        .collect(),
    }
}

#[test]
fn history_recalls_submitted_prompts_and_keeps_edits_out_of_previous_entries() {
    let mut chat = ready();
    send(&mut chat, "first");
    send(&mut chat, "second");
    chat.handle(Intent::Up);
    assert_eq!(chat.draft().parts(), ("second", ""));
    chat.handle(Intent::Up);
    assert_eq!(chat.draft().parts(), ("first", ""));
    chat.handle(Intent::Down);
    chat.handle(Intent::Down);
    assert_eq!(chat.draft().parts(), ("", ""));
    chat.handle(Intent::Up);
    chat.handle(Intent::Type('?'));
    chat.handle(Intent::Down);
    assert_eq!(chat.draft().parts(), ("second?", ""));
    assert_eq!(
        chat.handle(Intent::Submit),
        Some(Action::Send("second?".into()))
    );
    chat.apply(Event::Phase(Phase::Ready));
    chat.handle(Intent::Up);
    chat.handle(Intent::Up);
    assert_eq!(chat.draft().parts(), ("second", ""));
}

#[test]
fn arrows_edit_multiline_drafts_before_navigating_history() {
    let mut chat = ready();
    send(&mut chat, "one");
    send(&mut chat, "αβ\n猫犬");
    chat.handle(Intent::Up);
    chat.handle(Intent::Up);
    assert_eq!(chat.draft().parts(), ("αβ", "\n猫犬"));
    chat.handle(Intent::Down);
    assert_eq!(chat.draft().parts(), ("αβ\n猫犬", ""));
    chat.handle(Intent::Down);
    assert_eq!(chat.draft().parts(), ("", ""));

    chat.handle(Intent::Paste("draft\r\ntwo".into()));
    chat.handle(Intent::Up);
    chat.handle(Intent::Up);
    assert_eq!(chat.draft().parts(), ("dra", "ft\ntwo"));
}

#[test]
fn typing_opens_model_picker_and_paste_keeps_literal_command_lines() {
    let mut chat = ready();
    for c in "/mode".chars() {
        assert_eq!(chat.handle(Intent::Type(c)), None);
        assert!(chat.suggests_model());
    }
    assert_eq!(
        chat.handle(Intent::Type('l')),
        Some(Action::Send("/model".into()))
    );
    assert_eq!(chat.phase(), Phase::LoadingModels);
    chat.apply(Event::Phase(Phase::Ready));
    chat.handle(Intent::Up);
    assert_eq!(chat.draft().parts(), ("", ""));
    chat.handle(Intent::Type('/'));
    assert_eq!(
        chat.handle(Intent::Complete),
        Some(Action::Send("/model".into()))
    );
    chat.apply(Event::Phase(Phase::Ready));
    chat.handle(Intent::Paste("literal\r/model".into()));
    chat.handle(Intent::Newline);
    assert_eq!(
        chat.handle(Intent::Submit),
        Some(Action::Send("literal\n/model\n".into()))
    );
}

#[test]
fn picker_matches_name_and_id_and_sends_the_provider_id_after_filtering() {
    let mut chat = ready();
    chat.handle(Intent::Paste("follow-up".into()));
    chat.apply(picker(9));
    let selection = chat.picker().unwrap();
    assert_eq!(selection.selected(), Some(1));
    assert!(selection.is_current(selection.options().nth(1).unwrap()));
    chat.handle(Intent::Type('/'));
    chat.handle(Intent::Paste("8B".into()));
    let selection = chat.picker().unwrap();
    assert_eq!(selection.selected(), Some(0));
    assert_eq!(selection.options().next().unwrap().name, "Sage");
    chat.handle(Intent::Dismiss);
    assert_eq!(chat.picker().unwrap().options().len(), 3);
    chat.handle(Intent::Type('/'));
    for c in "deep".chars() {
        chat.handle(Intent::Type(c));
    }
    assert_eq!(
        chat.handle(Intent::Submit),
        Some(Action::AnswerModel(9, ModelChoice::Select("nova".into())))
    );
    assert_eq!(chat.draft().parts(), ("follow-up", ""));
    assert_eq!(chat.model(), "small");
    chat.apply(Event::ModelChanged("nova".into()));
    assert_eq!(chat.model(), "nova");
}

#[test]
fn empty_search_and_overlapping_requests_do_not_answer_the_wrong_form() {
    let mut chat = ready();
    for id in 1..=3 {
        chat.apply(picker(id));
    }
    chat.apply(Event::DismissModel(2));
    chat.handle(Intent::Type('z'));
    assert_eq!(chat.picker().unwrap().selected(), None);
    assert_eq!(chat.handle(Intent::Submit), None);
    chat.handle(Intent::Backspace);
    chat.apply(Event::Phase(Phase::Cancelled));
    assert_eq!(chat.handle(Intent::Dismiss), None);
    assert_eq!(
        chat.handle(Intent::Dismiss),
        Some(Action::AnswerModel(1, ModelChoice::Cancel))
    );
    chat.apply(Event::DismissModel(1));
    assert_eq!(
        chat.handle(Intent::Type('d')),
        Some(Action::AnswerModel(3, ModelChoice::Decline))
    );
    assert!(chat.picker().is_none());
}

#[test]
fn help_uses_an_empty_draft_and_keeps_normal_question_text() {
    let mut chat = ready();
    chat.handle(Intent::Type('?'));
    assert!(chat.help_open());
    chat.handle(Intent::Type('x'));
    chat.handle(Intent::Dismiss);
    assert!(!chat.help_open());
    assert_eq!(chat.draft().parts(), ("", ""));
    chat.handle(Intent::Type('x'));
    chat.handle(Intent::Type('?'));
    assert!(!chat.help_open());
    assert_eq!(chat.draft().parts(), ("x?", ""));
    chat.handle(Intent::Backspace);
    chat.handle(Intent::Backspace);
    chat.handle(Intent::Type('?'));
    chat.handle(Intent::Type('?'));
    assert!(!chat.help_open());
    assert_eq!(chat.draft().parts(), ("?", ""));
}

#[test]
fn message_edits_match_role_and_id_and_submit_waits_until_idle() {
    let mut chat = Chat::default();
    chat.handle(Intent::Paste("hello".into()));
    assert_eq!(chat.handle(Intent::Submit), None);
    chat.apply(Event::Phase(Phase::Ready));
    assert_eq!(
        chat.handle(Intent::Submit),
        Some(Action::Send("hello".into()))
    );
    chat.handle(Intent::Paste("next".into()));
    assert_eq!(chat.handle(Intent::Submit), None);
    chat.apply(Event::Message(
        Role::User,
        "1".into(),
        Edit::Replace("hello".into()),
    ));
    chat.apply(Event::Message(
        Role::Assistant,
        "1".into(),
        Edit::Append("hel".into()),
    ));
    chat.apply(Event::Message(
        Role::Assistant,
        "1".into(),
        Edit::Replace("hello!".into()),
    ));
    chat.apply(Event::Phase(Phase::Ready));
    assert_eq!(chat.entries().len(), 2);
    assert!(
        matches!(&chat.entries()[1], TranscriptEntry::Message { message, .. } if message.text == "hello!")
    );
    assert_eq!(
        chat.handle(Intent::Submit),
        Some(Action::Send("next".into()))
    );
}
