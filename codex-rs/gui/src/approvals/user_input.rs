//! `item/tool/requestUserInput`: every question on one card, answered with the
//! TUI's encoding (selected option label, then `user_note: ...` for typed
//! text; unanswered questions get an empty list; Skip sends no answers).

use std::collections::HashMap;

use codex_app_server_protocol::ToolRequestUserInputAnswer;
use codex_app_server_protocol::ToolRequestUserInputParams;
use codex_app_server_protocol::ToolRequestUserInputQuestion;
use codex_app_server_protocol::ToolRequestUserInputResponse;

use super::request::Answer;
use super::request::CardView;
use super::request::ChoiceView;
use super::request::FieldKind;
use super::request::FieldView;
use super::request::FormView;
use crate::transcript::NoticeKind;

/// Extra option added to questions that allow a free-form answer.
pub(crate) const OTHER_OPTION_LABEL: &str = "None of the above";
const OTHER_OPTION_DESCRIPTION: &str = "Describe what you want in the note below";
const NOTE_PREFIX: &str = "user_note: ";
/// Longest answer echoed into the transcript notice.
const NOTICE_ANSWER_CHARS: usize = 60;

#[derive(Clone, Debug)]
pub(crate) struct UserInputForm {
    questions: Vec<QuestionState>,
}

#[derive(Clone, Debug)]
struct QuestionState {
    question: ToolRequestUserInputQuestion,
    selected: Option<usize>,
    text: String,
}

impl QuestionState {
    fn options_len(&self) -> usize {
        self.question.options.as_ref().map_or(0, Vec::len)
    }

    fn has_options(&self) -> bool {
        self.options_len() > 0
    }

    fn other_enabled(&self) -> bool {
        self.question.is_other && self.has_options()
    }

    fn choice_count(&self) -> usize {
        self.options_len() + usize::from(self.other_enabled())
    }

    fn choice_label(&self, index: usize) -> Option<String> {
        let options = self.question.options.as_ref()?;
        if let Some(option) = options.get(index) {
            return Some(option.label.clone());
        }
        (index == options.len() && self.other_enabled()).then(|| OTHER_OPTION_LABEL.to_string())
    }

    fn other_selected(&self) -> bool {
        self.other_enabled() && self.selected == Some(self.options_len())
    }

    fn note(&self) -> &str {
        self.text.trim()
    }

    /// Encoded answers, exactly as the TUI submits them.
    fn answers(&self) -> Vec<String> {
        let mut answers = Vec::new();
        if self.has_options()
            && let Some(label) = self.selected.and_then(|index| self.choice_label(index))
        {
            answers.push(label);
        }
        if !self.note().is_empty() {
            answers.push(format!("{NOTE_PREFIX}{}", self.note()));
        }
        answers
    }

    fn is_answered(&self) -> bool {
        !self.answers().is_empty()
    }

    fn view(&self) -> FieldView {
        let choices = (0..self.choice_count())
            .map(|index| {
                let description = match self.question.options.as_ref().and_then(|o| o.get(index)) {
                    Some(option) => option.description.clone(),
                    None => OTHER_OPTION_DESCRIPTION.to_string(),
                };
                ChoiceView {
                    label: self.choice_label(index).unwrap_or_default(),
                    description,
                    checked: self.selected == Some(index),
                }
            })
            .collect();
        let placeholder = if !self.has_options() {
            "Type your answer"
        } else if self.other_selected() {
            "Describe what you want instead"
        } else {
            "Add a note (optional)"
        };
        FieldView {
            kind: if self.has_options() {
                FieldKind::SingleChoice
            } else {
                FieldKind::Text
            },
            header: self.question.header.clone(),
            prompt: self.question.question.clone(),
            required: false,
            choices,
            show_text: true,
            text: self.text.clone(),
            placeholder: placeholder.to_string(),
            secret: self.question.is_secret,
            error: String::new(),
        }
    }

    /// Answer as echoed in the transcript (secrets stay hidden).
    fn notice_text(&self) -> Option<String> {
        if !self.is_answered() {
            return None;
        }
        if self.question.is_secret {
            return Some("(hidden)".to_string());
        }
        let selected = if self.has_options() {
            self.selected.and_then(|index| self.choice_label(index))
        } else {
            None
        };
        let text = match (selected, self.note()) {
            (Some(label), "") => label,
            (Some(label), note) => format!("{label} ({note})"),
            (None, note) => note.to_string(),
        };
        Some(crate::app::truncate_chars(&text, NOTICE_ANSWER_CHARS))
    }
}

impl UserInputForm {
    pub(crate) fn new(params: &ToolRequestUserInputParams) -> Self {
        Self {
            questions: params
                .questions
                .iter()
                .map(|question| QuestionState {
                    question: question.clone(),
                    selected: None,
                    text: String::new(),
                })
                .collect(),
        }
    }

    /// Selects `choice` of `question`; returns whether anything changed.
    pub(crate) fn select(&mut self, question: usize, choice: usize) -> bool {
        let Some(state) = self.questions.get_mut(question) else {
            return false;
        };
        if choice >= state.choice_count() || state.selected == Some(choice) {
            return false;
        }
        state.selected = Some(choice);
        true
    }

    pub(crate) fn set_text(&mut self, question: usize, text: String) {
        if let Some(state) = self.questions.get_mut(question) {
            state.text = text;
        }
    }

    pub(crate) fn field(&self, question: usize) -> Option<FieldView> {
        self.questions.get(question).map(QuestionState::view)
    }

    /// Answers for every question (TUI encoding).
    pub(crate) fn response(&self) -> ToolRequestUserInputResponse {
        ToolRequestUserInputResponse {
            answers: self
                .questions
                .iter()
                .map(|state| {
                    (
                        state.question.id.clone(),
                        ToolRequestUserInputAnswer {
                            answers: state.answers(),
                        },
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn submit_answer(&self) -> serde_json::Result<Answer> {
        let answered: Vec<(String, String)> = self
            .questions
            .iter()
            .filter_map(|state| {
                state
                    .notice_text()
                    .map(|text| (state.question.header.clone(), text))
            })
            .collect();
        let text = match (self.questions.len(), answered.as_slice()) {
            (_, []) => "You submitted no answers".to_string(),
            (1, [(_, answer)]) => format!("You answered: {answer}"),
            (total, answered) => format!(
                "You answered {} of {total} questions: {}",
                answered.len(),
                answered
                    .iter()
                    .map(|(header, answer)| {
                        if header.is_empty() {
                            answer.clone()
                        } else {
                            format!("{header}: {answer}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        };
        Ok(Answer {
            result: serde_json::to_value(self.response())?,
            notice: Some((NoticeKind::Info, text)),
            open_url: None,
        })
    }

    /// Skip: an empty answer map, which the server accepts as "no answers".
    pub(crate) fn skip_answer(&self) -> serde_json::Result<Answer> {
        let response = ToolRequestUserInputResponse {
            answers: HashMap::new(),
        };
        let text = if self.questions.len() == 1 {
            "You skipped Codex's question"
        } else {
            "You skipped Codex's questions"
        };
        Ok(Answer {
            result: serde_json::to_value(response)?,
            notice: Some((NoticeKind::Info, text.to_string())),
            open_url: None,
        })
    }

    pub(crate) fn card(&self) -> CardView {
        let title = match self.questions.len() {
            1 => "Codex has a question".to_string(),
            count => format!("Codex has {count} questions"),
        };
        CardView::form(
            title,
            FormView {
                fields: self.questions.iter().map(QuestionState::view).collect(),
                submit_label: "Submit".to_string(),
                secondary_label: "Skip".to_string(),
                tertiary_label: String::new(),
                error: String::new(),
            },
        )
    }

    pub(crate) fn notification_body(&self) -> String {
        match self.questions.first() {
            Some(state) if self.questions.len() == 1 => {
                crate::app::truncate_chars(&state.question.question, /*max*/ 80)
            }
            _ => format!("Codex has {} questions", self.questions.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn form() -> UserInputForm {
        let params: ToolRequestUserInputParams = match serde_json::from_value(json!({
            "threadId": "t",
            "turnId": "u",
            "itemId": "i",
            "questions": [
                {
                    "id": "color",
                    "header": "Color",
                    "question": "Which color?",
                    "isOther": true,
                    "options": [
                        {"label": "Blue (Recommended)", "description": "Matches the theme"},
                        {"label": "Green", "description": "Stands out"},
                    ],
                },
                {"id": "name", "header": "Name", "question": "What name?", "options": null},
                {"id": "token", "header": "Token", "question": "Paste the token", "isSecret": true, "options": null},
            ],
        })) {
            Ok(params) => params,
            Err(err) => panic!("invalid test params: {err}"),
        };
        UserInputForm::new(&params)
    }

    fn answers(form: &UserInputForm) -> serde_json::Value {
        match serde_json::to_value(form.response()) {
            Ok(value) => value,
            Err(err) => panic!("encode failed: {err}"),
        }
    }

    #[test]
    fn missing_is_blocking_defaults_to_true() -> serde_json::Result<()> {
        let params: ToolRequestUserInputParams = serde_json::from_value(json!({
            "threadId": "t", "turnId": "u", "itemId": "i", "questions": [],
        }))?;
        assert!(params.is_blocking);
        Ok(())
    }

    #[test]
    fn unanswered_questions_get_empty_lists() {
        assert_eq!(
            answers(&form()),
            json!({"answers": {
                "color": {"answers": []},
                "name": {"answers": []},
                "token": {"answers": []},
            }})
        );
    }

    #[test]
    fn answers_use_labels_and_user_notes() {
        let mut form = form();
        assert!(form.select(/*question*/ 0, /*choice*/ 0));
        form.set_text(/*question*/ 0, "  but darker ".to_string());
        form.set_text(/*question*/ 1, "Ada".to_string());
        form.set_text(/*question*/ 2, "s3cret".to_string());
        assert_eq!(
            answers(&form),
            json!({"answers": {
                "color": {"answers": ["Blue (Recommended)", "user_note: but darker"]},
                "name": {"answers": ["user_note: Ada"]},
                "token": {"answers": ["user_note: s3cret"]},
            }})
        );
    }

    #[test]
    fn other_option_is_submitted_with_its_note() {
        let mut form = form();
        assert!(form.select(/*question*/ 0, /*choice*/ 2));
        form.set_text(/*question*/ 0, "Purple".to_string());
        let field = form
            .field(/*question*/ 0)
            .map(|field| (field.choices.len(), field.placeholder));
        assert_eq!(
            field,
            Some((3, "Describe what you want instead".to_string()))
        );
        assert_eq!(
            answers(&form)["answers"]["color"],
            json!({"answers": [OTHER_OPTION_LABEL, "user_note: Purple"]})
        );
        assert!(
            !form.select(/*question*/ 0, /*choice*/ 3),
            "out of range choice is ignored"
        );
    }

    #[test]
    fn skip_sends_an_empty_map() -> serde_json::Result<()> {
        let answer = form().skip_answer()?;
        assert_eq!(answer.result, json!({"answers": {}}));
        Ok(())
    }

    #[test]
    fn notice_hides_secrets() -> serde_json::Result<()> {
        let mut form = form();
        form.select(/*question*/ 0, /*choice*/ 1);
        form.set_text(/*question*/ 2, "s3cret".to_string());
        let answer = form.submit_answer()?;
        assert_eq!(
            answer.notice,
            Some((
                NoticeKind::Info,
                "You answered 2 of 3 questions: Color: Green; Token: (hidden)".to_string()
            ))
        );
        Ok(())
    }

    #[test]
    fn card_lists_every_question() {
        let card = form().card();
        assert_eq!(card.title, "Codex has 3 questions");
        let super::super::request::CardBody::Form(view) = card.body else {
            panic!("expected a form");
        };
        let kinds: Vec<(FieldKind, bool)> = view
            .fields
            .iter()
            .map(|field| (field.kind, field.secret))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (FieldKind::SingleChoice, false),
                (FieldKind::Text, false),
                (FieldKind::Text, true)
            ]
        );
        assert_eq!(view.secondary_label, "Skip");
    }
}
