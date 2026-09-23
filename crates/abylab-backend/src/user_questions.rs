//! Model-facing questions whose answers return to the same tool call.

use std::{collections::HashSet, sync::Arc};

use abycore::{CallBudget, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::contract::{Event, UserQuestion, UserQuestionReply};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    questions: Vec<UserQuestion>,
}

pub(crate) struct AskUserQuestionTool {
    sink: Arc<dyn Fn(Event) + Send + Sync>,
}

impl AskUserQuestionTool {
    pub fn new(sink: Arc<dyn Fn(Event) + Send + Sync>) -> Self {
        Self { sink }
    }

    fn parse(arguments: Value) -> Result<Request, ToolError> {
        let request: Request = serde_json::from_value(arguments)
            .map_err(|error| ToolError::Failed(format!("invalid questions: {error}")))?;
        if request.questions.is_empty() || request.questions.len() > 3 {
            return Err(failed("ask one to three questions"));
        }
        let mut ids = HashSet::new();
        for question in &request.questions {
            if !valid_text(&question.id, 64) || !ids.insert(&question.id) {
                return Err(failed("question ids must be unique and 1–64 characters"));
            }
            if !valid_text(&question.question, 1000)
                || question
                    .header
                    .as_deref()
                    .is_some_and(|s| !valid_text(s, 80))
            {
                return Err(failed("question or header is empty or too long"));
            }
            if question.options.len() > 8 {
                return Err(failed("a question may have at most eight options"));
            }
            let mut labels = HashSet::new();
            for option in &question.options {
                if !valid_text(&option.label, 100)
                    || !labels.insert(&option.label)
                    || option
                        .description
                        .as_deref()
                        .is_some_and(|s| !valid_text(s, 300))
                {
                    return Err(failed(
                        "option labels must be unique and nonempty; descriptions must be short",
                    ));
                }
            }
        }
        Ok(request)
    }

    async fn ask(&self, request: Request) -> Result<ToolOutput, ToolError> {
        let mut answers = Vec::with_capacity(request.questions.len());
        for question in request.questions {
            let (tx, rx) = oneshot::channel();
            (self.sink)(Event::UserQuestion {
                question: question.clone(),
                reply: tx,
            });
            let reply = rx
                .await
                .ok()
                .flatten()
                .ok_or_else(|| failed("user cancelled the question"))?;
            if !valid_reply(&question, &reply) {
                return Err(failed("invalid answer from the user interface"));
            }
            let mut answer = json!({
                "id": question.id,
                "selected": reply.selected,
            });
            if let Some(custom) = reply.custom {
                answer["custom"] = json!(custom);
            }
            answers.push(answer);
        }
        Ok(ToolOutput::text(json!({"answers": answers}).to_string()))
    }
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max
}

fn failed(message: &str) -> ToolError {
    ToolError::Failed(message.into())
}

fn valid_reply(question: &UserQuestion, reply: &UserQuestionReply) -> bool {
    (question.multi_select || reply.selected.len() <= 1)
        && reply
            .selected
            .iter()
            .all(|label| question.options.iter().any(|option| &option.label == label))
        && reply.selected.iter().collect::<HashSet<_>>().len() == reply.selected.len()
        && reply
            .custom
            .as_deref()
            .is_none_or(|text| valid_text(text, 2000))
        && (!reply.selected.is_empty() || reply.custom.is_some())
}

impl Tool for AskUserQuestionTool {
    fn call_budget(&self, _arguments: &Value) -> CallBudget {
        // Waiting for a human is not a stalled tool call. Turn cancellation
        // still drops the future and the UI's oneshot receiver.
        CallBudget::Unbounded
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user_question".into(),
            description: "Ask the user for a choice or missing information during the task. The call waits for their answer before continuing. Give each question a stable id; offer Yes/No or other options when useful. The user can always type a custom answer. Use multi_select when several options may be chosen.".into(),
            parameters: json!({
                "type":"object", "additionalProperties":false, "required":["questions"],
                "properties":{"questions":{
                    "type":"array", "minItems":1, "maxItems":3,
                    "items":{
                        "type":"object", "additionalProperties":false,
                        "required":["id","question"],
                        "properties":{
                            "id":{"type":"string","description":"Stable id echoed in the answer."},
                            "question":{"type":"string","description":"The question shown to the user."},
                            "header":{"type":"string","description":"Optional short heading."},
                            "options":{"type":"array","description":"Optional choices. Omit for a free-text question.","items":{
                                "type":"object", "additionalProperties":false,
                                "required":["label"],
                                "properties":{
                                    "label":{"type":"string"},
                                    "description":{"type":"string"}
                                }
                            }},
                            "multi_select":{"type":"boolean","description":"Allow several choices; defaults to false."}
                        }
                    }
                }}
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), ToolError> {
        Self::parse(arguments.clone()).map(|_| ())
    }

    fn execute<'a>(&'a self, arguments: Value, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let request = Self::parse(arguments)?;
            self.ask(request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_yes_no_and_multi_select_questions() {
        let input = json!({"questions":[
            {"id":"confirm","question":"Continue?","options":[{"label":"Yes"},{"label":"No"}]},
            {"id":"features","question":"Which features?","options":[{"label":"A"},{"label":"B"}],"multi_select":true}
        ]});
        assert!(AskUserQuestionTool::parse(input).is_ok());
        assert!(
            AskUserQuestionTool::parse(
                json!({"questions":[{"id":"x","question":"?"},{"id":"x","question":"?"}]})
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn answer_returns_to_the_model_and_cancel_is_an_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = AskUserQuestionTool::new(Arc::new(move |event| {
            tx.send(event).unwrap();
        }));
        let input = json!({"questions":[{"id":"go","question":"Go?","options":[{"label":"Yes"},{"label":"No"}]}]});
        let run = tool.ask(AskUserQuestionTool::parse(input.clone()).unwrap());
        let respond = async {
            let Event::UserQuestion { reply, .. } = rx.recv().await.unwrap() else {
                panic!("wrong event")
            };
            reply
                .send(Some(UserQuestionReply {
                    selected: vec!["Yes".into()],
                    custom: None,
                }))
                .unwrap();
        };
        let (answer, ()) = tokio::join!(run, respond);
        assert_eq!(
            serde_json::from_str::<Value>(&answer.unwrap().content).unwrap(),
            json!({"answers":[{"id":"go","selected":["Yes"]}]})
        );

        let run = tool.ask(AskUserQuestionTool::parse(input).unwrap());
        let cancel = async {
            let Event::UserQuestion { reply, .. } = rx.recv().await.unwrap() else {
                panic!("wrong event")
            };
            reply.send(None).unwrap();
        };
        let (result, ()) = tokio::join!(run, cancel);
        assert!(matches!(result, Err(ToolError::Failed(message)) if message.contains("cancelled")));
    }

    #[tokio::test]
    async fn several_questions_wait_for_each_answer_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = AskUserQuestionTool::new(Arc::new(move |event| {
            tx.send(event).unwrap();
        }));
        let request = AskUserQuestionTool::parse(json!({"questions":[
            {"id":"first","question":"Continue?","options":[{"label":"Yes"},{"label":"No"}]},
            {"id":"second","question":"Why?"}
        ]}))
        .unwrap();
        let run = tool.ask(request);
        let respond = async {
            let Event::UserQuestion { question, reply } = rx.recv().await.unwrap() else {
                panic!("wrong event")
            };
            assert_eq!(question.id, "first");
            assert!(
                rx.try_recv().is_err(),
                "the second question waits for the first answer"
            );
            reply
                .send(Some(UserQuestionReply {
                    selected: vec!["Yes".into()],
                    custom: None,
                }))
                .unwrap();
            let Event::UserQuestion { question, reply } = rx.recv().await.unwrap() else {
                panic!("wrong event")
            };
            assert_eq!(question.id, "second");
            reply
                .send(Some(UserQuestionReply {
                    selected: vec![],
                    custom: Some("Needed".into()),
                }))
                .unwrap();
        };
        let (result, ()) = tokio::join!(run, respond);
        assert_eq!(
            serde_json::from_str::<Value>(&result.unwrap().content).unwrap(),
            json!({"answers":[
                {"id":"first","selected":["Yes"]},
                {"id":"second","selected":[],"custom":"Needed"}
            ]})
        );
    }
}
