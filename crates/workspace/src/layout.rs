use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    FreeForm,
    TwoColumn,
    ThreeColumn,
}

impl std::fmt::Display for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Layout::FreeForm => write!(f, "free-form"),
            Layout::TwoColumn => write!(f, "2-columns"),
            Layout::ThreeColumn => write!(f, "3-columns"),
        }
    }
}

#[derive(Clone, Copy)]
pub enum LayoutRole {
    Terminal,
    AltTerminal,
    Editor,
    AltEditor,
}
