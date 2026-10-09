//! The "Use from AI tools" block on the Advanced page: the command that
//! registers this exe's `--mcp` server with Claude Code, and the JSON for
//! other MCP clients. Both copy buttons copy the exact path of the running exe.

use super::*;

impl super::SettingsApp {
    pub(crate) fn ai_tools_section(&mut self, ui: &mut egui::Ui) {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "quickdictate.exe".to_string());
        let command = format!("claude mcp add --scope user quickdictate -- \"{exe}\" --mcp");
        let config = serde_json::json!({
            "mcpServers": { "quickdictate": { "command": exe, "args": ["--mcp"] } }
        });
        let config = serde_json::to_string_pretty(&config).unwrap_or_default();

        ui.label(subsection_title("Use from AI tools"));
        ui.label(
            RichText::new(
                "QuickDictate can act as a tool for AI apps: they hand it an audio file and get \
                 the transcript back, using your installed local models or your API keys. It \
                 only runs while an AI app starts it.",
            )
            .size(12.0)
            .color(muted()),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new("Claude Code: run this once in a terminal")
                .size(12.0)
                .color(text()),
        );
        super::widgets::well(Margin::same(8)).show(ui, |ui| {
            ui.label(RichText::new(&command).monospace().size(12.0));
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new("Other MCP apps: add this to their config")
                .size(12.0)
                .color(text()),
        );
        super::widgets::well(Margin::same(8)).show(ui, |ui| {
            ui.label(RichText::new(&config).monospace().size(12.0));
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Copy command").clicked() {
                ui.ctx().copy_text(command.clone());
            }
            if ui.button("Copy JSON").clicked() {
                ui.ctx().copy_text(config.clone());
            }
        });
    }
}
