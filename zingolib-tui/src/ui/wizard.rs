use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::{inset, separator};
use crate::app::{Network, PassphrasePrompt, Wizard, WizardStep};
use crate::format;
use crate::theme::Theme;

const STEPS: [WizardStep; 5] = [
    WizardStep::Ufvk,
    WizardStep::Server,
    WizardStep::Birthday,
    WizardStep::Passphrase,
    WizardStep::Confirm,
];

/// A text field: a prompt marker, the value, and a block cursor.
fn field(t: &Theme, value: &str, masked: bool) -> Line<'static> {
    let shown = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    Line::from(vec![
        Span::styled("> ", t.accent),
        Span::styled(shown, t.text),
        Span::styled(" ", t.cursor),
    ])
}

fn hint(t: &Theme, pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, what)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, t.accent));
        spans.push(Span::styled(format!(" {what}"), t.muted));
    }
    Line::from(spans)
}

fn error(t: &Theme, message: &str) -> Line<'static> {
    Line::styled(message.to_string(), t.negative.add_modifier(Modifier::BOLD))
}

pub fn draw(frame: &mut Frame, area: Rect, t: &Theme, w: &Wizard) {
    let step_no = STEPS.iter().position(|s| *s == w.step).unwrap_or(0) + 1;
    let network = w.network.map_or("the network", Network::name);
    let header = super::edged(Rect { height: 1, ..area });
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("zingolib-tui", t.title),
            Span::styled("   new watch-only wallet", t.text),
        ])),
        header,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("step {step_no} of {}", STEPS.len()),
            t.muted,
        ))
        .alignment(ratatui::layout::Alignment::Right),
        header,
    );
    separator(
        frame,
        Rect {
            y: area.y + 1,
            height: 1,
            ..area
        },
        t,
    );
    let body = inset(Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    });
    let width = usize::from(body.width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match w.step {
        WizardStep::Ufvk => {
            lines.push(Line::styled("Paste the unified full viewing key.", t.text));
            lines.push(Line::styled(
                "Its prefix names the network: uview1 mainnet, uviewtest1 testnet, uviewregtest1 regtest. Nothing is sent anywhere.",
                t.muted,
            ));
            lines.push(Line::from(""));
            let wrapped = format::wrap_chars(&w.ufvk, width.saturating_sub(2));
            match wrapped.split_last() {
                Some((last, rest)) => {
                    for (i, l) in rest.iter().enumerate() {
                        lines.push(Line::from(vec![
                            if i == 0 {
                                Span::styled("> ", t.accent)
                            } else {
                                Span::raw("  ")
                            },
                            Span::styled(l.clone(), t.text),
                        ]));
                    }
                    let mut last_line = field(t, last, false);
                    if !rest.is_empty() {
                        last_line.spans[0] = Span::raw("  ");
                    }
                    lines.push(last_line);
                }
                None => lines.push(field(t, "", false)),
            }
            lines.push(Line::from(""));
            lines.push(hint(t, &[("Enter", "continue"), ("Esc", "quit")]));
        }
        WizardStep::Server => {
            lines.push(Line::styled(
                format!("Which {network} server should the wallet sync from?"),
                t.text,
            ));
            lines.push(Line::from(""));
            lines.push(field(t, &w.server, false));
            lines.push(Line::from(""));
            lines.push(hint(t, &[("Enter", "continue"), ("Esc", "back")]));
        }
        WizardStep::Birthday => {
            lines.push(Line::styled(
                "At which block height was the key created?",
                t.text,
            ));
            lines.push(Line::styled(
                "Blocks before this height are not scanned. Too high and old funds are missed. Too low and the first sync is slow.",
                t.muted,
            ));
            lines.push(Line::from(""));
            lines.push(field(t, &w.birthday, false));
            lines.push(Line::from(""));
            lines.push(hint(t, &[("Enter", "continue"), ("Esc", "back")]));
        }
        WizardStep::Passphrase => {
            lines.push(Line::styled(
                "Choose a passphrase to encrypt the wallet file, or leave it empty.",
                t.text,
            ));
            lines.push(Line::styled(
                "The file holds the viewing key, which reveals the whole transaction history.",
                t.muted,
            ));
            lines.push(Line::from(""));
            lines.push(field(t, &w.passphrase, true));
            lines.push(Line::from(""));
            lines.push(hint(t, &[("Enter", "continue"), ("Esc", "back")]));
        }
        WizardStep::Confirm => {
            lines.push(Line::styled("Type the passphrase again.", t.text));
            lines.push(Line::from(""));
            lines.push(field(t, &w.confirm, true));
            lines.push(Line::from(""));
            lines.push(hint(t, &[("Enter", "create wallet"), ("Esc", "back")]));
        }
    }
    if let Some(err) = &w.error {
        lines.push(Line::from(""));
        lines.push(error(t, err));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

pub fn draw_passphrase(frame: &mut Frame, area: Rect, t: &Theme, p: &PassphrasePrompt) {
    let mut lines = vec![
        Line::styled("The wallet file is encrypted.", t.title),
        Line::from(""),
        Line::styled("Passphrase", t.text),
        field(t, &p.input, true),
        Line::from(""),
        hint(t, &[("Enter", "unlock"), ("Esc", "quit")]),
    ];
    if let Some(err) = &p.error {
        lines.push(Line::from(""));
        lines.push(error(t, err));
    }
    frame.render_widget(Paragraph::new(lines), inset(area));
}
