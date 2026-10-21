//! Run history (`WorkflowRuns.tsx`): one page of 20 runs over the relay's
//! authenticated route, with its exact `(before, before_id)` cursor.

use super::controls;
use crate::mirror::mirror;
use buzz_relay_client::workflows::{
    RunCursor, RunPage, Runs, ViewSnapshot, ViewStatus, WorkflowDefinition, Workflows,
};
use buzz_ui::{ActiveTheme, Button, Surface, TypeStyled, tokens::space};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashSet;
use std::sync::Arc;

pub struct RunsView {
    workflows: Workflows,
    workflow: WorkflowDefinition,
    cursor: Option<RunCursor>,
    view: Option<Runs>,
    snapshot: Option<Arc<ViewSnapshot<RunPage>>>,
    error: Option<String>,
    /// Runs whose details and trace are open.
    open: HashSet<String>,
    _watch: Option<Task<()>>,
}

/// `2026-09-12T12:00:00.000Z`, as `Date.toISOString` writes it.
fn iso(seconds: u64) -> String {
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

impl RunsView {
    pub fn new(workflows: Workflows, workflow: WorkflowDefinition, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            workflows,
            workflow,
            cursor: None,
            view: None,
            snapshot: None,
            error: None,
            open: HashSet::new(),
            _watch: None,
        };
        this.page(None, cx);
        this
    }

    /// Reads a page: the newest, or the one before `cursor`.
    fn page(&mut self, cursor: Option<RunCursor>, cx: &mut Context<Self>) {
        self.cursor = cursor.clone();
        self.view = None;
        self._watch = None;
        self.snapshot = None;
        self.open.clear();
        match self.workflows.runs(&self.workflow, cursor) {
            Ok(view) => {
                drop(view.refresh(false));
                self.snapshot = Some(view.snapshot());
                self._watch = Some(mirror(view.watch(), cx, |this: &mut Self, value| {
                    this.snapshot = Some(value);
                }));
                self.view = Some(view);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}

impl Render for RunsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        if !self.workflows.availability().history {
            return div()
                .text_color(c.text_subtle)
                .child("Run history is unavailable from this host.")
                .into_any_element();
        }
        if let Some(error) = &self.error {
            return controls::danger(error.clone(), cx).into_any_element();
        }
        let Some(snapshot) = self.snapshot.clone() else {
            return div().child("Reading runs…").into_any_element();
        };
        let loading = snapshot.status == ViewStatus::Loading;
        let mut section = div().flex().flex_col().gap(space::S2).child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(space::S3)
                .my(space::S4)
                .child(div().text_heading().child("Runs"))
                .child(
                    Button::new("workflow-runs-refresh", "Refresh runs")
                        .surface(Surface::Floating)
                        .disabled(loading)
                        .on_click(cx.listener(|this, _, _, _| {
                            if let Some(view) = &this.view {
                                drop(view.refresh(false));
                            }
                        })),
                ),
        );
        let runs = &snapshot.data.runs;
        section = match snapshot.status {
            ViewStatus::Loading if runs.is_empty() => section.child(div().child("Reading runs…")),
            ViewStatus::Unavailable => {
                section.child(div().child(
                    "Run history is unavailable. This does not mean the workflow was deleted.",
                ))
            }
            ViewStatus::Idle => {
                section.child(div().child("History cleared. Refresh to read it again."))
            }
            ViewStatus::Error => section.child(controls::danger(
                snapshot.error.clone().unwrap_or_else(|| {
                    "Run history could not be read. Retry with Refresh runs.".into()
                }),
                cx,
            )),
            ViewStatus::Ready if runs.is_empty() => {
                section.child(div().child("No runs returned on this page."))
            }
            _ => section,
        };
        for run in runs {
            let open = self.open.contains(&run.id);
            let id = run.id.clone();
            let trace = serde_json::to_string_pretty(&run.trace).unwrap_or_default();
            section = section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S4)
                    .py(space::S4)
                    .border_b_1()
                    .border_color(c.border_standard)
                    .child(
                        div().text_body().child(
                            div()
                                .flex()
                                .gap(space::S1)
                                .child(
                                    div()
                                        .font_weight(FontWeight::BOLD)
                                        .child(run.status.label()),
                                )
                                .child(format!("· {}", iso(run.created_at))),
                        ),
                    )
                    .child(controls::note(
                        format!("Current step: {}", run.current_step),
                        cx,
                    ))
                    .when(
                        run.error_code.is_some() || run.error_message.is_some(),
                        |this| {
                            this.child(controls::danger(
                                format!(
                                    "{}: {}",
                                    run.error_code.clone().unwrap_or_default(),
                                    run.error_message.clone().unwrap_or_default()
                                ),
                                cx,
                            ))
                        },
                    )
                    .child(controls::disclosure(
                        SharedString::from(format!("workflow-run-{}", run.id)),
                        "Run details and trace",
                        open,
                        cx.listener(move |this, _, _, cx| {
                            if !this.open.remove(&id) {
                                this.open.insert(id.clone());
                            }
                            cx.notify();
                        }),
                        cx,
                    ))
                    .when(open, |this| {
                        this.child(div().text_mono().child(format!("Run ID: {}", run.id)))
                            .child(
                                div()
                                    .id(SharedString::from(format!("workflow-trace-{}", run.id)))
                                    .max_h(rems(25.))
                                    .overflow_y_scroll()
                                    .p(space::S3)
                                    .bg(c.surface_inset)
                                    .text_mono()
                                    .child(trace),
                            )
                    }),
            );
        }
        let next = (snapshot.status == ViewStatus::Ready)
            .then(|| snapshot.data.next.clone())
            .flatten();
        section
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(space::S3)
                    .my(space::S4)
                    .when(self.cursor.is_some(), |this| {
                        this.child(
                            Button::new("workflow-runs-newest", "Newest runs")
                                .surface(Surface::Floating)
                                .on_click(cx.listener(|this, _, _, cx| this.page(None, cx))),
                        )
                    })
                    .when_some(next, |this, next| {
                        this.child(
                            Button::new("workflow-runs-older", "Older runs")
                                .surface(Surface::Floating)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.page(Some(next.clone()), cx)
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn writes_times_as_iso_strings() {
        assert_eq!(iso(1_789_224_000), "2026-09-12T14:40:00.000Z");
    }
}
