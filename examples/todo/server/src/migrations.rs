//! Data migrations. Pending ones run with `todo-server serve --migrate`, `todo-server migrate`, or from the
//! dashboard's Migrations page (which also does dry runs).

use ferrix::prelude::*;

use crate::todos::Todo;

/// Trims the text of todos written before `add` trimmed it (e.g. imported or edited in the dashboard). Todos that are
/// already trimmed are left alone, so running it again changes nothing.
#[migration(table = Todo, name = "0001_trim_todo_text")]
pub async fn trim_todo_text(_ctx: &MutationCtx, todo: Doc<Todo>) -> Result<DocChange> {
    let trimmed = todo.text.trim();
    Ok(if trimmed == todo.text { DocChange::Keep } else { DocChange::Patch(json!({ "text": trimmed })) })
}
