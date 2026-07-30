// app.rs — application state, phrases as top-level units

use crate::command::{self, Cmd, JinsSpec, LlmProvider, ValueChange};
use crate::fx::FxSettings;
use crate::record;
use crate::sequencer::{build_control_entry, build_phrase, AudioCmd, BarSpec, ControlSpec, Phrase};
use crate::vcf::{VcfBank, VcfSettings, VcfTarget, VcoWave};
use crossbeam_channel::Sender;
use std::fs;
use std::path::{Path, PathBuf};

pub struct App {
    pub phrases: Vec<Phrase>,
    pub input: String,
    pub message: Option<String>,
    pub show_help: bool,
    pub show_jins: bool,
    pub help_scroll: u16,
    pub jins_scroll: u16,
    pub bpm: f64,
    pub sustain: f64,
    pub vcf: VcfBank,
    pub fx: FxSettings,
    pub vol: f32,
    pub paused: bool,
    pub should_quit: bool,
    pub last_recording: Option<String>,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    pub saved_input: String,
    pub cursor_pos: usize,
    pub rec_rx: Option<crossbeam_channel::Receiver<Result<String, String>>>,
    pub llm_rx: Option<crossbeam_channel::Receiver<Result<String, String>>>,
    session_path: Option<String>,
    next_phrase_id: usize,
    last_rhythm: Vec<u8>,
    auditioning_jins: bool,
    audio_tx: Sender<AudioCmd>,
    /// Sender to push BPM updates into the clockout thread (None if not started)
    clockout_tx: Option<crossbeam_channel::Sender<f64>>,
}

impl App {
    pub fn new(audio_tx: Sender<AudioCmd>) -> Self {
        App {
            phrases: Vec::new(),
            input: String::new(),
            message: Some("? for help".into()),
            show_help: false,
            show_jins: false,
            help_scroll: 0,
            jins_scroll: 0,
            bpm: 120.0,
            sustain: 1.25,
            vcf: VcfBank::default(),
            fx: FxSettings::default(),
            vol: 1.0,
            paused: false,
            should_quit: false,
            last_recording: None,
            history: Vec::new(),
            history_pos: None,
            saved_input: String::new(),
            cursor_pos: 0,
            rec_rx: None,
            llm_rx: None,
            session_path: None,
            next_phrase_id: 0,
            last_rhythm: vec![3, 3, 2],
            auditioning_jins: false,
            audio_tx,
            clockout_tx: None,
        }
    }

    // ── History ───────────────────────────────────────────────────────────

    pub fn history_push(&mut self, cmd: &str) {
        let s = cmd.trim().to_string();
        if !s.is_empty() && self.history.last().map(|x| x.as_str()) != Some(&s) {
            self.history.push(s);
        }
        self.history_pos = None;
        self.saved_input.clear();
    }

    pub fn last_history(&self) -> Option<&str> {
        self.history.last().map(|s| s.as_str())
    }

    pub fn session_filename(&self) -> Option<&str> {
        self.session_path.as_deref().and_then(|path| {
            Path::new(path)
                .file_name()
                .and_then(|filename| filename.to_str())
        })
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_pos {
            None => {
                self.saved_input = self.input.clone();
                self.history_pos = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(i) => {
                self.history_pos = Some(i - 1);
            }
        }
        if let Some(i) = self.history_pos {
            self.input = self.history[i].clone();
            self.cursor_pos = self.input.chars().count();
        }
    }

    pub fn history_down(&mut self) {
        match self.history_pos {
            None => {}
            Some(i) if i + 1 >= self.history.len() => {
                self.history_pos = None;
                self.input = self.saved_input.clone();
                self.cursor_pos = self.input.chars().count();
            }
            Some(i) => {
                self.history_pos = Some(i + 1);
                self.input = self.history[i + 1].clone();
                self.cursor_pos = self.input.chars().count();
            }
        }
    }

    // ── Cursor / line editing ─────────────────────────────────────────────

    pub fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
        }
    }

    pub fn cursor_right(&mut self) {
        let n = self.input.chars().count();
        if self.cursor_pos < n {
            self.cursor_pos += 1;
        }
    }

    pub fn cursor_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor_pos = self.input.chars().count();
    }

    pub fn insert_char(&mut self, ch: char) {
        let byte_pos: usize = self
            .input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len());
        self.input.insert(byte_pos, ch);
        self.cursor_pos += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let byte_pos: usize = self
            .input
            .char_indices()
            .nth(self.cursor_pos - 1)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len().saturating_sub(1));
        self.input.remove(byte_pos);
        self.cursor_pos -= 1;
    }

    pub fn delete_char(&mut self) {
        let n = self.input.chars().count();
        if self.cursor_pos >= n {
            return;
        }
        let byte_pos: usize = self
            .input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len());
        self.input.remove(byte_pos);
    }

    pub fn complete_input(&mut self) {
        if self.complete_edit_input() {
            return;
        }
        if self.complete_metadata_command_input() {
            return;
        }
        if self.complete_phrase_input() {
            return;
        }
        let Some((cmd, arg_start, partial)) = completion_target(&self.input) else {
            return;
        };
        let matches = mq_matches(cmd, &partial);
        if matches.is_empty() {
            self.message = Some("✗ no .mq matches".into());
            return;
        }

        let common = completion_common_prefix(cmd, &partial, &matches);
        let replacement = if matches.len() == 1 {
            matches[0].clone()
        } else if common.len() > partial.len() {
            self.message = Some(format!("{}: {}", cmd, matches.join("  ")));
            common
        } else {
            self.message = Some(format!("{}: {}", cmd, matches.join("  ")));
            return;
        };

        self.input
            .replace_range(arg_start..self.input.len(), &replacement);
        self.cursor_pos = self.input.chars().count();
        if matches.len() == 1 {
            self.message = None;
        }
    }

    fn complete_edit_input(&mut self) -> bool {
        let trimmed = self.input.trim();
        let mut tokens = trimmed.split_whitespace();
        if tokens.next() != Some("edit") {
            return false;
        }
        let Some(id_token) = tokens.next() else {
            return false;
        };
        if tokens.next().is_some() {
            return false;
        }
        let Ok(id_ref) = id_token.parse::<isize>() else {
            return false;
        };
        let Some(id) = self.resolve_id_ref(id_ref) else {
            self.message = Some(format!("✗ no phrase id {id_ref}"));
            return true;
        };
        let Some(phrase) = self.phrases.iter().find(|phrase| phrase.id == id) else {
            self.message = Some(format!("✗ no phrase id {id}"));
            return true;
        };
        self.input = format!("edit {id_token} {}", phrase.display_src());
        self.cursor_pos = self.input.chars().count();
        self.message = None;
        true
    }

    fn complete_metadata_command_input(&mut self) -> bool {
        let Some((body_start, body)) = command_body_for_completion(&self.input) else {
            return false;
        };
        let Some(completion) = metadata_command_completion(body) else {
            return false;
        };
        if let Some(replacement) = completion.replacement {
            self.input
                .replace_range(body_start..self.input.len(), &replacement);
            self.cursor_pos = self.input.chars().count();
        }
        self.message = completion.message;
        true
    }

    fn complete_phrase_input(&mut self) -> bool {
        let Some(replacement) = phrase_completion(&self.input, &self.phrases) else {
            return false;
        };
        self.input = replacement;
        self.cursor_pos = self.input.chars().count();
        self.message = None;
        true
    }

    pub fn overlay_scroll_up(&mut self) {
        if self.show_help {
            self.help_scroll = self.help_scroll.saturating_sub(1);
        } else if self.show_jins {
            self.jins_scroll = self.jins_scroll.saturating_sub(1);
        }
    }

    pub fn overlay_scroll_down(&mut self) {
        if self.show_help {
            self.help_scroll = self.help_scroll.saturating_add(1);
        } else if self.show_jins {
            self.jins_scroll = self.jins_scroll.saturating_add(1);
        }
    }

    pub fn overlay_scroll_home(&mut self) {
        if self.show_help {
            self.help_scroll = 0;
        }
        if self.show_jins {
            self.jins_scroll = 0;
        }
    }

    // ── Render thread poll ────────────────────────────────────────────────

    pub fn tick(&mut self) {
        if let Some(rx) = &self.llm_rx {
            if let Ok(result) = rx.try_recv() {
                self.message = Some(match result {
                    Ok(answer) => answer,
                    Err(e) => format!("✗ {e}"),
                });
                self.llm_rx = None;
            }
        }
        if let Some(rx) = &self.rec_rx {
            if let Ok(result) = rx.try_recv() {
                match result {
                    Ok(path) => {
                        self.last_recording = Some(path.clone());
                        self.message = Some(format!("saved → {path}"));
                    }
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                    }
                }
                self.rec_rx = None;
            }
        }
    }

    fn resync_audio_sequence(&mut self, focus_id: Option<usize>) {
        let target_pos = focus_id
            .and_then(|id| self.phrases.iter().position(|p| p.id == id))
            .unwrap_or(0);
        let (start_bpm, start_sustain, start_vcf, start_fx) = self.sequence_start_settings();
        let _ = self.audio_tx.send(AudioCmd::Clear);
        let _ = self.audio_tx.send(AudioCmd::SetBpm(start_bpm));
        let _ = self.audio_tx.send(AudioCmd::SetSustain(start_sustain));
        let _ = self.audio_tx.send(AudioCmd::SetVcfBank(start_vcf));
        let _ = self.audio_tx.send(AudioCmd::SetFxSettings(start_fx));
        let _ = self.audio_tx.send(AudioCmd::SetVol(self.vol));
        let _ = self.audio_tx.send(AudioCmd::SetPaused(self.paused));
        for p in self.phrases.iter().cloned() {
            let _ = self.audio_tx.send(AudioCmd::AddPhrase(p));
        }
        if !self.phrases.is_empty() {
            let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(
                target_pos.min(self.phrases.len() - 1),
            ));
        }
        self.auditioning_jins = false;
    }

    fn resolve_id_ref(&self, id_ref: isize) -> Option<usize> {
        resolve_id_ref_in_phrases(&self.phrases, id_ref)
    }

    fn insert_sym_control(&mut self, before: isize, src: String, control: ControlSpec) {
        let insert_pos = match self.resolve_id_ref(before) {
            Some(before_id) => self
                .phrases
                .iter()
                .position(|phrase| phrase.id == before_id)
                .unwrap_or(self.phrases.len()),
            None => {
                self.message = Some(format!("✗ no phrase id {before}"));
                return;
            }
        };
        let id = self.next_phrase_id;
        self.next_phrase_id += 1;
        let entry = build_control_entry(id, src, control);
        self.phrases.insert(insert_pos, entry.clone());
        let _ = self.audio_tx.send(AudioCmd::InsertPhrase {
            pos: insert_pos,
            phrase: entry,
        });
        self.message = Some(format!("inserted sym at {insert_pos}"));
    }

    fn replace_sym_control(&mut self, id_ref: isize, src: String, control: ControlSpec) {
        let Some(id) = self.resolve_id_ref(id_ref) else {
            self.message = Some(format!("✗ no phrase id {id_ref}"));
            return;
        };
        let Some(pos) = self.phrases.iter().position(|phrase| phrase.id == id) else {
            self.message = Some(format!("✗ no phrase id {id}"));
            return;
        };
        let entry = build_control_entry(id, src, control);
        self.phrases[pos] = entry.clone();
        let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(entry));
        self.message = Some(format!("edited {id} → sym"));
    }

    fn sequence_start_settings(&self) -> (f64, f64, VcfBank, FxSettings) {
        let mut bpm = 120.0f64;
        let mut sustain = 1.25f64;
        let mut vcf = VcfBank::default();
        let mut fx = FxSettings::default();
        for phrase in &self.phrases {
            if let Some(ctrl) = phrase.control {
                match ctrl {
                    ControlSpec::Stop => {}
                    ControlSpec::SetBpm(v) => bpm = v,
                    ControlSpec::SetSustain(v) => sustain = v,
                    ControlSpec::SetVcf(v) => {
                        if let Ok(setting) = command::apply_vcf_change(vcf, v) {
                            vcf.apply(setting);
                        }
                    }
                    ControlSpec::SetFx(v) => {
                        if let Ok(setting) = command::apply_fx_change(fx, v) {
                            fx = setting;
                        }
                    }
                    ControlSpec::SetSympathetics(_)
                    | ControlSpec::SetSympatheticDecay(_)
                    | ControlSpec::SetSympatheticGain(_)
                    | ControlSpec::SetSympathetic(_) => {}
                }
                continue;
            }
            if phrase.jump.is_none() {
                break;
            }
        }
        (bpm, sustain, vcf, fx)
    }

    fn audition_jins(&mut self, specs: Vec<JinsSpec>) -> Result<(), String> {
        let resolved = resolve_rhythms(specs, &[1])?;
        let src = resolved
            .iter()
            .map(|s| s.src.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut phrase = build_phrase(usize::MAX, format!("[preview] {src}"), resolved, 4, 1);
        let n_freqs = phrase.bar.frequencies.len().max(1);
        let mut walk = Vec::with_capacity(n_freqs * 4);
        for degree in 0..n_freqs {
            walk.push(degree);
            walk.push(degree);
        }
        if n_freqs > 1 {
            for degree in (0..(n_freqs - 1)).rev() {
                walk.push(degree);
                walk.push(degree);
            }
        }
        phrase.bar.groups = vec![1; walk.len()];
        phrase.bar.group_degrees = walk;
        phrase.bar.group_degrees.push(0);
        phrase.bar.recompute_events();
        phrase.bar.total_subdivs = phrase.bar.events.len();

        self.paused = false;
        let _ = self.audio_tx.send(AudioCmd::Clear);
        let _ = self.audio_tx.send(AudioCmd::SetBpm(self.bpm));
        let _ = self.audio_tx.send(AudioCmd::SetSustain(self.sustain));
        let _ = self.audio_tx.send(AudioCmd::SetVcfBank(self.vcf));
        let _ = self.audio_tx.send(AudioCmd::SetFxSettings(self.fx));
        let _ = self.audio_tx.send(AudioCmd::SetVol(self.vol));
        let _ = self.audio_tx.send(AudioCmd::SetPaused(false));
        let _ = self.audio_tx.send(AudioCmd::AddPhrase(phrase));
        let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(0));
        self.auditioning_jins = true;
        Ok(())
    }

    // ── Commands ──────────────────────────────────────────────────────────

    pub fn handle_command(&mut self, raw: &str) {
        for part in raw.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }

            // clockin <device> — receive MIDI clock, sync BPM to external gear
            if let Some(dev) = part.strip_prefix("clockin ") {
                let dev = dev.trim().to_string();
                crate::midi_clock::start_clock_receiver(dev.clone(), self.audio_tx.clone());
                self.message = Some(format!("clock ← {dev}"));
                continue;
            }

            // clockout <device> — send MIDI clock, slave external gear to maqam-live BPM
            if let Some(dev) = part.strip_prefix("clockout ") {
                let dev = dev.trim().to_string();
                let tx = crate::midi_clockout::start_clock_sender(dev.clone(), self.bpm);
                self.clockout_tx = Some(tx);
                self.message = Some(format!("clock → {dev}"));
                continue;
            }

            match command::parse(part) {
                Ok(cmd) => self.execute(cmd),
                Err(msg) => {
                    self.message = Some(format!("✗ {msg}"));
                    return;
                }
            }
        }
    }

    fn execute(&mut self, cmd: Cmd) {
        let keep_audition = matches!(
            &cmd,
            Cmd::CreateJins { .. } | Cmd::AuditionJins { .. } | Cmd::Help | Cmd::ListJins
        );
        if self.auditioning_jins && !keep_audition {
            self.resync_audio_sequence(None);
        }
        match cmd {
            Cmd::Quit => self.should_quit = true,
            Cmd::Help => {
                self.show_help = true;
            }
            Cmd::AskLlm { provider, prompt } => {
                self.ask_llm(provider, prompt);
            }
            Cmd::Jump { to, times } => {
                let Some(to) = self.resolve_id_ref(to) else {
                    self.message = Some(format!("✗ no phrase id {to}"));
                    return;
                };
                if !self.phrases.iter().any(|p| p.id == to) {
                    self.message = Some(format!("✗ no phrase id {to}"));
                    return;
                }
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = crate::sequencer::build_jump_entry(id, to, times);
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry.clone()));
                self.phrases.push(entry);
                self.message = None;
            }

            Cmd::Insert {
                before,
                source,
                specs,
                repeat,
            } => {
                if specs.is_empty() {
                    self.message = Some("✗ empty phrase".into());
                    return;
                }
                let resolved = match resolve_rhythms(specs, &self.last_rhythm) {
                    Ok(r) => r,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                if let Some(r) = resolved.last() {
                    self.last_rhythm = r.groups.clone();
                }
                let peak = 4usize;
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let phrase = build_phrase(id, source, resolved, peak, repeat.max(1));
                let pos = match self.resolve_id_ref(before) {
                    Some(before_id) => self
                        .phrases
                        .iter()
                        .position(|p| p.id == before_id)
                        .unwrap_or(self.phrases.len()),
                    None => {
                        self.message = Some(format!("✗ no phrase id {before}"));
                        return;
                    }
                };
                self.phrases.insert(pos, phrase.clone());
                let _ = self.audio_tx.send(AudioCmd::InsertPhrase { pos, phrase });
                self.message = Some(format!("inserted at {pos}"));
            }

            Cmd::InsertJump { before, to, times } => {
                let Some(to) = self.resolve_id_ref(to) else {
                    self.message = Some(format!("✗ no phrase id {to}"));
                    return;
                };
                if !self.phrases.iter().any(|p| p.id == to) {
                    self.message = Some(format!("✗ no phrase id {to}"));
                    return;
                }
                let insert_pos = match self.resolve_id_ref(before) {
                    Some(before_id) => self
                        .phrases
                        .iter()
                        .position(|p| p.id == before_id)
                        .unwrap_or(self.phrases.len()),
                    None => {
                        self.message = Some(format!("✗ no phrase id {before}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = crate::sequencer::build_jump_entry(id, to, times);
                self.phrases.insert(insert_pos, entry.clone());
                let _ = self.audio_tx.send(AudioCmd::InsertPhrase {
                    pos: insert_pos,
                    phrase: entry,
                });
                self.message = None;
            }

            Cmd::InsertBpm { before, change } => {
                let bpm = match apply_bpm_change(self.bpm, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let insert_pos = match self.resolve_id_ref(before) {
                    Some(before_id) => self
                        .phrases
                        .iter()
                        .position(|p| p.id == before_id)
                        .unwrap_or(self.phrases.len()),
                    None => {
                        self.message = Some(format!("✗ no phrase id {before}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = build_control_entry(id, format!("bpm {bpm}"), ControlSpec::SetBpm(bpm));
                self.phrases.insert(insert_pos, entry.clone());
                let _ = self.audio_tx.send(AudioCmd::InsertPhrase {
                    pos: insert_pos,
                    phrase: entry,
                });
                self.message = Some(format!("inserted bpm at {insert_pos}"));
            }

            Cmd::InsertSustain { before, change } => {
                let secs = match apply_sustain_change(self.sustain, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let insert_pos = match self.resolve_id_ref(before) {
                    Some(before_id) => self
                        .phrases
                        .iter()
                        .position(|p| p.id == before_id)
                        .unwrap_or(self.phrases.len()),
                    None => {
                        self.message = Some(format!("✗ no phrase id {before}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry =
                    build_control_entry(id, format!("s {secs}"), ControlSpec::SetSustain(secs));
                self.phrases.insert(insert_pos, entry.clone());
                let _ = self.audio_tx.send(AudioCmd::InsertPhrase {
                    pos: insert_pos,
                    phrase: entry,
                });
                self.message = Some(format!("inserted sustain at {insert_pos}"));
            }

            Cmd::InsertVcf { before, change } => {
                let _vcf = match command::apply_vcf_change(self.vcf, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let insert_pos = match self.resolve_id_ref(before) {
                    Some(before_id) => self
                        .phrases
                        .iter()
                        .position(|p| p.id == before_id)
                        .unwrap_or(self.phrases.len()),
                    None => {
                        self.message = Some(format!("✗ no phrase id {before}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry =
                    build_control_entry(id, vcf_change_src(change), ControlSpec::SetVcf(change));
                self.phrases.insert(insert_pos, entry.clone());
                let _ = self.audio_tx.send(AudioCmd::InsertPhrase {
                    pos: insert_pos,
                    phrase: entry,
                });
                self.message = Some(format!("inserted vcf at {insert_pos}"));
            }

            Cmd::InsertFx { before, change } => {
                let fx = match command::apply_fx_change(self.fx, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let insert_pos = match self.resolve_id_ref(before) {
                    Some(before_id) => self
                        .phrases
                        .iter()
                        .position(|p| p.id == before_id)
                        .unwrap_or(self.phrases.len()),
                    None => {
                        self.message = Some(format!("✗ no phrase id {before}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry =
                    build_control_entry(id, fx_change_src(change), ControlSpec::SetFx(change));
                self.phrases.insert(insert_pos, entry.clone());
                let _ = self.audio_tx.send(AudioCmd::InsertPhrase {
                    pos: insert_pos,
                    phrase: entry,
                });
                self.message = Some(format!("inserted {}", describe_fx(fx)));
            }

            Cmd::InsertSympathetics { before, enabled } => {
                self.insert_sym_control(
                    before,
                    if enabled { "sym on" } else { "sym off" }.into(),
                    ControlSpec::SetSympathetics(enabled),
                );
            }

            Cmd::InsertSympatheticDecay { before, decay } => {
                self.insert_sym_control(
                    before,
                    format!("sym decay {decay}"),
                    ControlSpec::SetSympatheticDecay(decay),
                );
            }

            Cmd::InsertSympatheticGain { before, gain } => {
                self.insert_sym_control(
                    before,
                    format!("sym drive {gain}"),
                    ControlSpec::SetSympatheticGain(gain),
                );
            }

            Cmd::InsertSympathetic { before, change } => {
                self.insert_sym_control(
                    before,
                    sym_change_src(change),
                    ControlSpec::SetSympathetic(change),
                );
            }

            Cmd::TogglePause { start_id } => {
                if let Some(id) = start_id {
                    let Some(id) = self.resolve_id_ref(id) else {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    };
                    // z <id>: queue the address for the next phrase exit.
                    match self.phrases.iter().position(|p| p.id == id) {
                        Some(_) => {
                            let _ = self.audio_tx.send(AudioCmd::QueueNextPhrase(id));
                            self.message = Some(format!("next → phrase {id}"));
                        }
                        None => {
                            self.message = Some(format!("✗ no phrase id {id}"));
                        }
                    }
                } else {
                    // z alone: toggle pause; restart from 0 when unpausing
                    self.paused = !self.paused;
                    if !self.paused {
                        let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(0));
                    }
                    let _ = self.audio_tx.send(AudioCmd::SetPaused(self.paused));
                    self.message = Some(if self.paused {
                        "⏸ paused".into()
                    } else {
                        "▶ playing".into()
                    });
                }
            }

            Cmd::SetVol(v) => {
                self.vol = v;
                let _ = self.audio_tx.send(AudioCmd::SetVol(v));
                self.message = Some(format!("vol → {v:.2}"));
            }

            Cmd::Record(reps) => {
                if crate::REC_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
                    self.message = Some("✗ already rendering".into());
                    return;
                }
                let phrases = self.phrases.clone();
                let (bpm, sustain, vcf, fx) = self.sequence_start_settings();
                let (tx, rx) = crossbeam_channel::bounded(1);
                self.rec_rx = Some(rx);
                self.message = Some(format!("◉ rendering {}×…", reps));
                std::thread::spawn(move || {
                    let result = record::record_cycle(phrases, bpm, sustain, vcf, fx, reps)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(result);
                });
            }

            Cmd::Rotate => {
                if self.phrases.len() < 2 {
                    self.message = Some("nothing to rotate".into());
                } else {
                    let first = self.phrases.remove(0);
                    self.phrases.push(first);
                    let _ = self.audio_tx.send(AudioCmd::Rotate);
                    self.message = None;
                }
            }

            Cmd::MoveUp(id) => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let Some(pos) = self.phrases.iter().position(|p| p.id == id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                if pos == 0 {
                    self.message = Some(format!("id {id} is already at top"));
                    return;
                }
                self.phrases.swap(pos - 1, pos);
                let _ = self.audio_tx.send(AudioCmd::MovePhrase { id, down: false });
                self.message = Some(format!("moved {id} up"));
            }

            Cmd::MoveDown(id) => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let Some(pos) = self.phrases.iter().position(|p| p.id == id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                if pos + 1 >= self.phrases.len() {
                    self.message = Some(format!("id {id} is already at bottom"));
                    return;
                }
                self.phrases.swap(pos, pos + 1);
                let _ = self.audio_tx.send(AudioCmd::MovePhrase { id, down: true });
                self.message = Some(format!("moved {id} down"));
            }

            Cmd::ListJins => {
                self.show_jins = true;
            }

            Cmd::AuditionJins { specs } => {
                let label = specs
                    .iter()
                    .map(|s| s.src.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                match self.audition_jins(specs) {
                    Ok(()) => self.message = Some(format!("auditioning {label}")),
                    Err(e) => self.message = Some(format!("✗ {e}")),
                }
            }

            Cmd::CreateJins { name, ratios } => match crate::tuning::Maqam::create(&name, ratios) {
                Ok(()) => self.message = Some(format!("created jins {name}")),
                Err(e) => self.message = Some(format!("✗ {e}")),
            },

            Cmd::DeleteJins { name } => {
                if crate::tuning::Maqam::delete(&name) {
                    self.message = Some(format!("deleted jins {name}"));
                } else {
                    self.message = Some(format!("✗ no jins '{name}'"));
                }
            }

            Cmd::Save { path } => {
                let path = match path.or_else(|| self.session_path.clone()) {
                    Some(path) => path,
                    None => {
                        self.message = Some("✗ usage: save <path>".into());
                        return;
                    }
                };
                match self.save_session(&path) {
                    Ok(()) => {
                        self.session_path = Some(path.clone());
                        self.message = Some(format!("saved session → {path}"));
                    }
                    Err(e) => self.message = Some(format!("✗ save failed: {e}")),
                }
            }

            Cmd::Load { path } => match self.load_session(&path) {
                Ok(()) => {
                    self.session_path = Some(path.clone());
                    self.message = Some(format!("loaded session ← {path}"));
                }
                Err(e) => self.message = Some(format!("✗ load failed: {e}")),
            },

            Cmd::Clear => {
                self.phrases.clear();
                self.next_phrase_id = 0;
                let _ = self.audio_tx.send(AudioCmd::Clear);
                self.message = Some("cleared".into());
            }
            Cmd::Stop => {
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = build_control_entry(id, "stop".into(), ControlSpec::Stop);
                self.phrases.push(entry.clone());
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                self.message = Some("stop line added".into());
            }
            Cmd::Sympathetics(enabled) => {
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let src = if enabled { "sym on" } else { "sym off" };
                let entry =
                    build_control_entry(id, src.into(), ControlSpec::SetSympathetics(enabled));
                self.phrases.push(entry.clone());
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetSympathetics(enabled));
                self.message = Some(if enabled {
                    "sympathetic strings on".into()
                } else {
                    "sympathetic strings off".into()
                });
            }
            Cmd::SympatheticDecay(decay) => {
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = build_control_entry(
                    id,
                    format!("sym decay {decay}"),
                    ControlSpec::SetSympatheticDecay(decay),
                );
                self.phrases.push(entry.clone());
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetSympatheticDecay(decay));
                self.message = Some(format!("sym decay {decay:.5}"));
            }
            Cmd::SympatheticGain(gain) => {
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = build_control_entry(
                    id,
                    format!("sym gain {gain}"),
                    ControlSpec::SetSympatheticGain(gain),
                );
                self.phrases.push(entry.clone());
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetSympatheticGain(gain));
                self.message = Some(format!("sym gain {gain:.2}"));
            }
            Cmd::Sympathetic(change) => {
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let src = sym_change_src(change);
                let entry =
                    build_control_entry(id, src.clone(), ControlSpec::SetSympathetic(change));
                self.phrases.push(entry.clone());
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetSympathetic(change));
                self.message = Some(src);
            }
            Cmd::SetBpm(change) => {
                let bpm = match apply_bpm_change(self.bpm, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry = build_control_entry(id, format!("bpm {bpm}"), ControlSpec::SetBpm(bpm));
                self.phrases.push(entry.clone());
                self.bpm = bpm;
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetBpm(bpm));
                self.push_clockout_bpm(bpm);
                self.message = Some(format!("BPM line → {bpm:.2}"));
            }
            Cmd::SetSustain(change) => {
                let secs = match apply_sustain_change(self.sustain, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry =
                    build_control_entry(id, format!("s {secs}"), ControlSpec::SetSustain(secs));
                self.phrases.push(entry.clone());
                self.sustain = secs;
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetSustain(secs));
                self.message = Some(format!("s line → {secs:.2}s"));
            }
            Cmd::SetVcf(change) => {
                let vcf = match command::apply_vcf_change(self.vcf, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry =
                    build_control_entry(id, vcf_change_src(change), ControlSpec::SetVcf(change));
                self.phrases.push(entry.clone());
                self.vcf.apply(vcf);
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetVcf(change));
                self.message = Some(format!("VCF line → {}", describe_vcf(vcf)));
            }
            Cmd::SetFx(change) => {
                let fx = match command::apply_fx_change(self.fx, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let entry =
                    build_control_entry(id, fx_change_src(change), ControlSpec::SetFx(change));
                self.phrases.push(entry.clone());
                self.fx = fx;
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(entry));
                let _ = self.audio_tx.send(AudioCmd::SetFx(change));
                self.message = Some(format!("FX line → {}", describe_fx(fx)));
            }

            Cmd::EditJump { id, to, times } => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let Some(to) = self.resolve_id_ref(to) else {
                    self.message = Some(format!("✗ no phrase id {to}"));
                    return;
                };
                if !self.phrases.iter().any(|p| p.id == to) {
                    self.message = Some(format!("✗ no phrase id {to}"));
                    return;
                }
                let pos = match self.phrases.iter().position(|p| p.id == id) {
                    Some(p) => p,
                    None => {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    }
                };
                let mut entry = crate::sequencer::build_jump_entry(id, to, times);
                entry.id = id; // preserve the original id
                self.phrases[pos] = entry.clone();
                let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(entry));
                self.message = Some(format!("edited {id} → jump to {to} ×{times}"));
            }

            Cmd::Edit {
                id,
                source,
                specs,
                repeat,
            } => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let pos = match self.phrases.iter().position(|p| p.id == id) {
                    Some(p) => p,
                    None => {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    }
                };
                let resolved = match resolve_rhythms(specs, &self.last_rhythm) {
                    Ok(r) => r,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                if let Some(r) = resolved.last() {
                    self.last_rhythm = r.groups.clone();
                }
                let mut phrase = build_phrase(id, source, resolved, 4, repeat.max(1));
                phrase.id = id;
                self.phrases[pos] = phrase.clone();
                let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(phrase));
                // Editing establishes the score-design position. Move playback
                // to the edited phrase so the TUI immediately marks it current
                // and the audio thread can publish its jump-aware successor.
                crate::CUR_PHRASE.store(pos, std::sync::atomic::Ordering::Relaxed);
                crate::CUR_SUBDIV.store(0, std::sync::atomic::Ordering::Relaxed);
                crate::CUR_PLAYS.store(0, std::sync::atomic::Ordering::Relaxed);
                crate::NEXT_PHRASE.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
                crate::EXIT_PHRASE.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
                let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(pos));
                self.message = Some(format!("edited {id}"));
            }

            Cmd::EditBpm { id, change } => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let pos = match self.phrases.iter().position(|p| p.id == id) {
                    Some(p) => p,
                    None => {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    }
                };
                let bpm = match apply_bpm_change(self.bpm, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let entry = build_control_entry(id, format!("bpm {bpm}"), ControlSpec::SetBpm(bpm));
                self.phrases[pos] = entry.clone();
                let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(entry));
                self.message = Some(format!("edited {id} → bpm {bpm:.2}"));
            }

            Cmd::EditSustain { id, change } => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let pos = match self.phrases.iter().position(|p| p.id == id) {
                    Some(p) => p,
                    None => {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    }
                };
                let secs = match apply_sustain_change(self.sustain, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let entry =
                    build_control_entry(id, format!("s {secs}"), ControlSpec::SetSustain(secs));
                self.phrases[pos] = entry.clone();
                let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(entry));
                self.message = Some(format!("edited {id} → s {secs:.2}s"));
            }

            Cmd::EditVcf { id, change } => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let pos = match self.phrases.iter().position(|p| p.id == id) {
                    Some(p) => p,
                    None => {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    }
                };
                let vcf = match command::apply_vcf_change(self.vcf, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let entry =
                    build_control_entry(id, vcf_change_src(change), ControlSpec::SetVcf(change));
                self.phrases[pos] = entry.clone();
                let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(entry));
                self.message = Some(format!("edited {id} → {}", describe_vcf(vcf)));
            }

            Cmd::EditFx { id, change } => {
                let Some(id) = self.resolve_id_ref(id) else {
                    self.message = Some(format!("✗ no phrase id {id}"));
                    return;
                };
                let pos = match self.phrases.iter().position(|p| p.id == id) {
                    Some(p) => p,
                    None => {
                        self.message = Some(format!("✗ no phrase id {id}"));
                        return;
                    }
                };
                let fx = match command::apply_fx_change(self.fx, change) {
                    Ok(v) => v,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                let entry =
                    build_control_entry(id, fx_change_src(change), ControlSpec::SetFx(change));
                self.phrases[pos] = entry.clone();
                let _ = self.audio_tx.send(AudioCmd::ReplacePhrase(entry));
                self.message = Some(format!("edited {id} → {}", describe_fx(fx)));
            }

            Cmd::EditSympathetics { id, enabled } => {
                self.replace_sym_control(
                    id,
                    if enabled { "sym on" } else { "sym off" }.into(),
                    ControlSpec::SetSympathetics(enabled),
                );
            }

            Cmd::EditSympatheticDecay { id, decay } => {
                self.replace_sym_control(
                    id,
                    format!("sym decay {decay}"),
                    ControlSpec::SetSympatheticDecay(decay),
                );
            }

            Cmd::EditSympatheticGain { id, gain } => {
                self.replace_sym_control(
                    id,
                    format!("sym gain {gain}"),
                    ControlSpec::SetSympatheticGain(gain),
                );
            }

            Cmd::EditSympathetic { id, change } => {
                self.replace_sym_control(
                    id,
                    sym_change_src(change),
                    ControlSpec::SetSympathetic(change),
                );
            }

            Cmd::DeleteBars(ids) => {
                let mut not_found = Vec::new();
                for id_ref in &ids {
                    let Some(id) = self.resolve_id_ref(*id_ref) else {
                        not_found.push(*id_ref);
                        continue;
                    };
                    if let Some(pos) = self.phrases.iter().position(|p| p.id == id) {
                        let removed = self.phrases.remove(pos);
                        let _ = self.audio_tx.send(AudioCmd::RemovePhrase(removed.id));
                    } else {
                        not_found.push(*id_ref);
                    }
                }
                if !not_found.is_empty() {
                    let s: Vec<String> = not_found.iter().map(|i| i.to_string()).collect();
                    self.message = Some(format!("✗ no id {}", s.join(" ")));
                } else {
                    self.message = None;
                }
            }

            Cmd::AddPhrase {
                source,
                specs,
                repeat,
            } => {
                if specs.is_empty() {
                    self.message = Some("✗ empty phrase".into());
                    return;
                }
                let resolved = match resolve_rhythms(specs, &self.last_rhythm) {
                    Ok(r) => r,
                    Err(e) => {
                        self.message = Some(format!("✗ {e}"));
                        return;
                    }
                };
                if let Some(r) = resolved.last() {
                    self.last_rhythm = r.groups.clone();
                }
                let peak: usize = if self.phrases.is_empty() {
                    4
                } else {
                    let total: usize = self.phrases.iter().map(|p| p.bar.total_subdivs).sum();
                    let count = self.phrases.len().max(1);
                    (total / count / 2).clamp(2, 4)
                };
                let id = self.next_phrase_id;
                self.next_phrase_id += 1;
                let phrase = build_phrase(id, source, resolved, peak, repeat.max(1));
                let _ = self.audio_tx.send(AudioCmd::AddPhrase(phrase.clone()));
                self.phrases.push(phrase);
                self.message = None;
            }
        }
    }

    /// Push a BPM update to the clockout thread if it's running.
    fn push_clockout_bpm(&self, bpm: f64) {
        if let Some(tx) = &self.clockout_tx {
            let _ = tx.send(bpm);
        }
    }

    fn ask_llm(&mut self, provider: LlmProvider, prompt: String) {
        if self.llm_rx.is_some() {
            self.message = Some("✗ already asking LLM".into());
            return;
        }
        let Some(request) = LlmRequest::from_env(provider, prompt) else {
            self.message = Some(match provider {
                LlmProvider::ChatGpt => {
                    "✗ environment variable OPENAI_API_KEY needs to be set to talk to chatgpt"
                        .into()
                }
                LlmProvider::Claude => {
                    "✗ environment variable ANTHROPIC_API_KEY or CLAUDE_API_KEY needs to be set to talk to claude"
                        .into()
                }
            });
            return;
        };
        let provider_name = request.provider_name();
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.llm_rx = Some(rx);
        self.message = Some(format!("asking {provider_name}..."));
        std::thread::spawn(move || {
            let result = request.send();
            let _ = tx.send(result);
        });
    }

    fn save_session(&self, path: &str) -> Result<(), String> {
        let out = crate::session_v3::serialize_session_v3(&self.phrases);
        fs::write(path, out).map_err(|e| e.to_string())
    }

    fn load_session(&mut self, path: &str) -> Result<(), String> {
        let src = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut lines = src.lines();
        let Some(header) = lines.next() else {
            return Err("empty file".into());
        };
        let header = header.trim();
        if header == crate::session_v3::HEADER {
            return self.load_session_v3(lines);
        }
        if header == "MAQAM_SESSION_V2" {
            return self.load_session_v2(lines);
        }
        if header == "MAQAM_SESSION_V1" {
            return self.load_session_v1(lines);
        }
        Err("bad header (expected MAQAM_SESSION_V3, MAQAM_SESSION_V2, or MAQAM_SESSION_V1)".into())
    }

    fn load_session_v3<'a, I>(&mut self, lines: I) -> Result<(), String>
    where
        I: Iterator<Item = &'a str>,
    {
        crate::tuning::Maqam::reset_to_defaults();
        let mut new_bpm = 120.0f64;
        let mut new_sustain = 1.25f64;
        let mut new_vcf = VcfBank::default();
        let mut new_fx = FxSettings::default();
        let new_vol = self.vol;
        let mut loaded: Vec<Phrase> = Vec::new();
        let mut ids = std::collections::HashSet::new();
        let mut max_id = None;
        let mut next_legacy_id = 0usize;
        let mut last_rhythm = vec![3, 3, 2];

        for (line_idx, raw_line) in lines.enumerate() {
            let line_no = line_idx + 2;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with("create ") {
                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                let Cmd::CreateJins { name, ratios } = parsed else {
                    return Err(format!("line {line_no}: expected create line"));
                };
                crate::tuning::Maqam::create(&name, ratios)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                continue;
            }
            if line.starts_with("vol ") {
                continue;
            }
            if is_plain_control_line(line) {
                while ids.contains(&next_legacy_id) {
                    next_legacy_id += 1;
                }
                let id = next_legacy_id;
                next_legacy_id += 1;
                ids.insert(id);
                max_id = Some(max_id.map_or(id, |current: usize| current.max(id)));

                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                match parsed {
                    Cmd::SetBpm(change) => {
                        new_bpm = apply_bpm_change(new_bpm, change)
                            .map_err(|e| format!("line {line_no}: {e}"))?;
                        loaded.push(build_control_entry(
                            id,
                            format!("bpm {new_bpm}"),
                            ControlSpec::SetBpm(new_bpm),
                        ));
                    }
                    Cmd::SetSustain(change) => {
                        new_sustain = apply_sustain_change(new_sustain, change)
                            .map_err(|e| format!("line {line_no}: {e}"))?;
                        loaded.push(build_control_entry(
                            id,
                            format!("s {new_sustain}"),
                            ControlSpec::SetSustain(new_sustain),
                        ));
                    }
                    Cmd::SetVcf(change) => {
                        let setting = command::apply_vcf_change(new_vcf, change)
                            .map_err(|e| format!("line {line_no}: {e}"))?;
                        new_vcf.apply(setting);
                        loaded.push(build_control_entry(
                            id,
                            vcf_change_src(change),
                            ControlSpec::SetVcf(change),
                        ));
                    }
                    Cmd::SetFx(change) => {
                        new_fx = command::apply_fx_change(new_fx, change)
                            .map_err(|e| format!("line {line_no}: {e}"))?;
                        loaded.push(build_control_entry(
                            id,
                            fx_change_src(change),
                            ControlSpec::SetFx(change),
                        ));
                    }
                    Cmd::Sympathetics(enabled) => {
                        loaded.push(build_control_entry(
                            id,
                            if enabled { "sym on" } else { "sym off" }.into(),
                            ControlSpec::SetSympathetics(enabled),
                        ));
                    }
                    Cmd::SympatheticDecay(decay) => {
                        loaded.push(build_control_entry(
                            id,
                            format!("sym decay {decay}"),
                            ControlSpec::SetSympatheticDecay(decay),
                        ));
                    }
                    Cmd::SympatheticGain(gain) => {
                        loaded.push(build_control_entry(
                            id,
                            format!("sym gain {gain}"),
                            ControlSpec::SetSympatheticGain(gain),
                        ));
                    }
                    Cmd::Sympathetic(change) => {
                        loaded.push(build_control_entry(
                            id,
                            sym_change_src(change),
                            ControlSpec::SetSympathetic(change),
                        ));
                    }
                    _ => return Err(format!("line {line_no}: expected control line")),
                }
                continue;
            }

            let fields = crate::session_v3::split_escaped_fields(line);
            let id = fields
                .get(1)
                .ok_or_else(|| format!("line {line_no}: missing id"))?
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("line {line_no}: bad id"))?;
            if !ids.insert(id) {
                return Err(format!("line {line_no}: duplicate id {id}"));
            }
            max_id = Some(max_id.map_or(id, |current: usize| current.max(id)));

            match fields.first().map(String::as_str) {
                Some("T") if fields.len() == 3 && fields[2].trim() == "stop" => {
                    loaded.push(build_control_entry(id, "stop".into(), ControlSpec::Stop));
                }
                Some("B") if fields.len() == 3 => {
                    new_bpm = fields[2]
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| format!("line {line_no}: bad bpm"))?;
                    if !(20.0..=400.0).contains(&new_bpm) {
                        return Err(format!("line {line_no}: bpm out of range"));
                    }
                    loaded.push(build_control_entry(
                        id,
                        format!("bpm {new_bpm}"),
                        ControlSpec::SetBpm(new_bpm),
                    ));
                }
                Some("S") if fields.len() == 3 => {
                    new_sustain = fields[2]
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| format!("line {line_no}: bad sustain"))?;
                    if !(0.05..=10.0).contains(&new_sustain) {
                        return Err(format!("line {line_no}: sustain out of range"));
                    }
                    loaded.push(build_control_entry(
                        id,
                        format!("s {new_sustain}"),
                        ControlSpec::SetSustain(new_sustain),
                    ));
                }
                Some("V") if fields.len() == 3 => {
                    let parsed =
                        command::parse(&fields[2]).map_err(|e| format!("line {line_no}: {e}"))?;
                    let Cmd::SetVcf(change) = parsed else {
                        return Err(format!("line {line_no}: expected vcf line"));
                    };
                    let setting = command::apply_vcf_change(new_vcf, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    new_vcf.apply(setting);
                    loaded.push(build_control_entry(
                        id,
                        vcf_change_src(change),
                        ControlSpec::SetVcf(change),
                    ));
                }
                Some("F") if fields.len() == 3 => {
                    let parsed =
                        command::parse(&fields[2]).map_err(|e| format!("line {line_no}: {e}"))?;
                    let Cmd::SetFx(change) = parsed else {
                        return Err(format!("line {line_no}: expected fx line"));
                    };
                    new_fx = command::apply_fx_change(new_fx, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    loaded.push(build_control_entry(
                        id,
                        fx_change_src(change),
                        ControlSpec::SetFx(change),
                    ));
                }
                Some("Y") if fields.len() == 3 => {
                    let parsed =
                        command::parse(&fields[2]).map_err(|e| format!("line {line_no}: {e}"))?;
                    let (src, control) = match parsed {
                        Cmd::Sympathetics(enabled) => (
                            if enabled { "sym on" } else { "sym off" }.to_string(),
                            ControlSpec::SetSympathetics(enabled),
                        ),
                        Cmd::SympatheticDecay(decay) => (
                            format!("sym decay {decay}"),
                            ControlSpec::SetSympatheticDecay(decay),
                        ),
                        Cmd::SympatheticGain(gain) => (
                            format!("sym gain {gain}"),
                            ControlSpec::SetSympatheticGain(gain),
                        ),
                        Cmd::Sympathetic(change) => {
                            (sym_change_src(change), ControlSpec::SetSympathetic(change))
                        }
                        _ => return Err(format!("line {line_no}: expected sym line")),
                    };
                    loaded.push(build_control_entry(id, src, control));
                }
                Some("V") if (5..=8).contains(&fields.len()) => {
                    let (target, offset) = if fields.len() >= 6 {
                        let target = VcfTarget::parse(&fields[2])
                            .ok_or_else(|| format!("line {line_no}: bad vcf target"))?;
                        (target, 3)
                    } else {
                        (new_vcf.focus, 2)
                    };
                    let cutoff_hz = fields[offset]
                        .trim()
                        .parse::<f32>()
                        .map_err(|_| format!("line {line_no}: bad vcf cutoff"))?;
                    let resonance = fields[offset + 1]
                        .trim()
                        .parse::<f32>()
                        .map_err(|_| format!("line {line_no}: bad vcf resonance"))?;
                    let drive = fields[offset + 2]
                        .trim()
                        .parse::<f32>()
                        .map_err(|_| format!("line {line_no}: bad vcf drive"))?;
                    let enabled = if fields.len() >= 7 {
                        match fields[6].trim().to_ascii_lowercase().as_str() {
                            "on" | "true" | "1" => true,
                            "off" | "false" | "0" => false,
                            _ => return Err(format!("line {line_no}: bad vcf enabled flag")),
                        }
                    } else {
                        true
                    };
                    let wave = if fields.len() == 8 {
                        VcoWave::parse(&fields[7])
                            .ok_or_else(|| format!("line {line_no}: bad vcf wave"))?
                    } else {
                        new_vcf.get(target).wave
                    };
                    let change = command::VcfChange {
                        enabled: Some(enabled),
                        target: Some(target),
                        cutoff_hz: Some(ValueChange::Set(cutoff_hz as f64)),
                        resonance: Some(ValueChange::Set(resonance as f64)),
                        drive: Some(ValueChange::Set(drive as f64)),
                        wave: Some(wave),
                    };
                    let setting = command::apply_vcf_change(new_vcf, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    new_vcf.apply(setting);
                    loaded.push(build_control_entry(
                        id,
                        vcf_change_src(change),
                        ControlSpec::SetVcf(change),
                    ));
                }
                Some("J") if fields.len() == 4 => {
                    let target = fields[2]
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| format!("line {line_no}: bad jump target"))?;
                    let times = fields[3]
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| format!("line {line_no}: bad jump times"))?;
                    loaded.push(crate::sequencer::build_jump_entry(id, target, times.max(1)));
                }
                Some("P") if fields.len() == 4 => {
                    let repeat = fields[2]
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| format!("line {line_no}: bad repeat"))?
                        .max(1);
                    let src = &fields[3];
                    let parsed = command::parse(src).map_err(|e| format!("line {line_no}: {e}"))?;
                    let Cmd::AddPhrase { specs, .. } = parsed else {
                        return Err(format!("line {line_no}: expected phrase command"));
                    };
                    let resolved = resolve_rhythms(specs, &last_rhythm)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    if let Some(rhythm) = resolved.last() {
                        last_rhythm = rhythm.groups.clone();
                    }
                    loaded.push(build_phrase(id, src.clone(), resolved, 4, repeat));
                }
                _ => return Err(format!("line {line_no}: malformed V3 record")),
            }
        }

        self.phrases = loaded.clone();
        self.next_phrase_id = max_id.map_or(0, |id| id.saturating_add(1));
        self.last_rhythm = last_rhythm;
        self.bpm = new_bpm;
        self.sustain = new_sustain;
        self.vcf = new_vcf;
        self.fx = new_fx;
        self.vol = new_vol;
        self.paused = false;
        let (start_bpm, start_sustain, start_vcf, start_fx) = self.sequence_start_settings();

        let _ = self.audio_tx.send(AudioCmd::Clear);
        let _ = self.audio_tx.send(AudioCmd::SetBpm(start_bpm));
        let _ = self.audio_tx.send(AudioCmd::SetSustain(start_sustain));
        let _ = self.audio_tx.send(AudioCmd::SetVcfBank(start_vcf));
        let _ = self.audio_tx.send(AudioCmd::SetFxSettings(start_fx));
        let _ = self.audio_tx.send(AudioCmd::SetVol(self.vol));
        let _ = self.audio_tx.send(AudioCmd::SetPaused(false));
        for phrase in loaded {
            let _ = self.audio_tx.send(AudioCmd::AddPhrase(phrase));
        }
        let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(0));
        Ok(())
    }

    fn load_session_v1<'a, I>(&mut self, lines: I) -> Result<(), String>
    where
        I: Iterator<Item = &'a str>,
    {
        crate::tuning::Maqam::reset_to_defaults();
        let mut new_bpm = self.bpm;
        let mut new_sustain = self.sustain;
        let mut new_vcf = self.vcf;
        let mut new_fx = self.fx;
        let new_vol = self.vol;
        let mut loaded: Vec<Phrase> = Vec::new();
        let mut max_id = 0usize;
        let mut last_rhythm = vec![3, 3, 2];

        for (line_idx, raw_line) in lines.enumerate() {
            let line_no = line_idx + 2;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if line.starts_with("create ") {
                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                let Cmd::CreateJins { name, ratios } = parsed else {
                    return Err(format!("line {line_no}: expected create line"));
                };
                crate::tuning::Maqam::create(&name, ratios)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                continue;
            }

            if line.starts_with("bpm ") {
                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                let Cmd::SetBpm(change) = parsed else {
                    return Err(format!("line {line_no}: expected bpm line"));
                };
                new_bpm = apply_bpm_change(new_bpm, change)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                let entry = build_control_entry(
                    max_id,
                    format!("bpm {new_bpm}"),
                    ControlSpec::SetBpm(new_bpm),
                );
                loaded.push(entry);
                max_id += 1;
                continue;
            }
            if line.starts_with("s ") || line.starts_with("sus ") {
                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                let Cmd::SetSustain(change) = parsed else {
                    return Err(format!("line {line_no}: expected sustain line"));
                };
                new_sustain = apply_sustain_change(new_sustain, change)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                let entry = build_control_entry(
                    max_id,
                    format!("s {new_sustain}"),
                    ControlSpec::SetSustain(new_sustain),
                );
                loaded.push(entry);
                max_id += 1;
                continue;
            }
            if is_plain_vcf_control_line(line) {
                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                let Cmd::SetVcf(change) = parsed else {
                    return Err(format!("line {line_no}: expected vcf line"));
                };
                let setting = command::apply_vcf_change(new_vcf, change)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                new_vcf.apply(setting);
                let entry = build_control_entry(
                    max_id,
                    vcf_change_src(change),
                    ControlSpec::SetVcf(change),
                );
                loaded.push(entry);
                max_id += 1;
                continue;
            }
            if is_plain_fx_control_line(line) {
                let parsed = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
                let Cmd::SetFx(change) = parsed else {
                    return Err(format!("line {line_no}: expected fx line"));
                };
                new_fx = command::apply_fx_change(new_fx, change)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                let entry =
                    build_control_entry(max_id, fx_change_src(change), ControlSpec::SetFx(change));
                loaded.push(entry);
                max_id += 1;
                continue;
            }
            if line.starts_with("vol ") {
                continue;
            }

            if let Some(payload) = line.strip_prefix("J|") {
                let mut parts = payload.splitn(3, '|');
                let id = parts
                    .next()
                    .ok_or(format!("line {line_no}: missing jump id"))?
                    .parse::<usize>()
                    .map_err(|_| format!("line {line_no}: bad jump id"))?;
                let target = parts
                    .next()
                    .ok_or(format!("line {line_no}: missing jump target"))?
                    .parse::<usize>()
                    .map_err(|_| format!("line {line_no}: bad jump target"))?;
                let times = parts
                    .next()
                    .ok_or(format!("line {line_no}: missing jump times"))?
                    .parse::<usize>()
                    .map_err(|_| format!("line {line_no}: bad jump times"))?;
                max_id = max_id.max(id);
                loaded.push(crate::sequencer::build_jump_entry(id, target, times.max(1)));
                continue;
            }

            if let Some(payload) = line.strip_prefix("P|") {
                let mut parts = payload.splitn(3, '|');
                let id = parts
                    .next()
                    .ok_or(format!("line {line_no}: missing phrase id"))?
                    .parse::<usize>()
                    .map_err(|_| format!("line {line_no}: bad phrase id"))?;
                let repeat = parts
                    .next()
                    .ok_or(format!("line {line_no}: missing repeat"))?
                    .parse::<usize>()
                    .map_err(|_| format!("line {line_no}: bad repeat"))?;
                let src = parts
                    .next()
                    .ok_or(format!("line {line_no}: missing phrase source"))?;
                let cmd_src = if repeat > 1 {
                    format!("{src} r{repeat}")
                } else {
                    src.to_string()
                };
                let parsed =
                    command::parse(&cmd_src).map_err(|e| format!("line {line_no}: {e}"))?;
                let (specs, rep) = match parsed {
                    Cmd::AddPhrase { specs, repeat, .. } => (specs, repeat),
                    _ => return Err(format!("line {line_no}: expected phrase command")),
                };
                let resolved = resolve_rhythms(specs, &last_rhythm)
                    .map_err(|e| format!("line {line_no}: {e}"))?;
                if let Some(r) = resolved.last() {
                    last_rhythm = r.groups.clone();
                }
                let phrase = build_phrase(id, src.to_string(), resolved, 4, rep.max(1));
                max_id = max_id.max(id);
                loaded.push(phrase);
                continue;
            }

            return Err(format!("line {line_no}: unrecognized line"));
        }

        self.phrases = loaded.clone();
        self.next_phrase_id = max_id.saturating_add(1);
        self.last_rhythm = last_rhythm;
        self.bpm = new_bpm;
        self.sustain = new_sustain;
        self.vcf = new_vcf;
        self.fx = new_fx;
        self.vol = new_vol;
        self.paused = false;
        let (start_bpm, start_sustain, start_vcf, start_fx) = self.sequence_start_settings();

        let _ = self.audio_tx.send(AudioCmd::Clear);
        let _ = self.audio_tx.send(AudioCmd::SetBpm(start_bpm));
        let _ = self.audio_tx.send(AudioCmd::SetSustain(start_sustain));
        let _ = self.audio_tx.send(AudioCmd::SetVcfBank(start_vcf));
        let _ = self.audio_tx.send(AudioCmd::SetFxSettings(start_fx));
        let _ = self.audio_tx.send(AudioCmd::SetVol(self.vol));
        let _ = self.audio_tx.send(AudioCmd::SetPaused(false));
        for p in loaded {
            let _ = self.audio_tx.send(AudioCmd::AddPhrase(p));
        }
        let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(0));
        Ok(())
    }

    fn load_session_v2<'a, I>(&mut self, lines: I) -> Result<(), String>
    where
        I: Iterator<Item = &'a str>,
    {
        crate::tuning::Maqam::reset_to_defaults();
        let mut new_bpm = 120.0f64;
        let mut new_sustain = 1.25f64;
        let mut new_vcf = VcfBank::default();
        let mut new_fx = FxSettings::default();
        let new_vol = self.vol;
        let mut loaded: Vec<Phrase> = Vec::new();
        let mut next_id = 0usize;
        let mut last_rhythm = vec![3, 3, 2];

        for (line_idx, raw_line) in lines.enumerate() {
            let line_no = line_idx + 2;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cmd = command::parse(line).map_err(|e| format!("line {line_no}: {e}"))?;
            match cmd {
                Cmd::SetBpm(change) => {
                    new_bpm = apply_bpm_change(new_bpm, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    let entry = build_control_entry(
                        next_id,
                        format!("bpm {new_bpm}"),
                        ControlSpec::SetBpm(new_bpm),
                    );
                    next_id += 1;
                    loaded.push(entry);
                }
                Cmd::SetSustain(change) => {
                    new_sustain = apply_sustain_change(new_sustain, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    let entry = build_control_entry(
                        next_id,
                        format!("s {new_sustain}"),
                        ControlSpec::SetSustain(new_sustain),
                    );
                    next_id += 1;
                    loaded.push(entry);
                }
                Cmd::SetVcf(change) => {
                    let setting = command::apply_vcf_change(new_vcf, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    new_vcf.apply(setting);
                    let entry = build_control_entry(
                        next_id,
                        vcf_change_src(change),
                        ControlSpec::SetVcf(change),
                    );
                    next_id += 1;
                    loaded.push(entry);
                }
                Cmd::SetFx(change) => {
                    new_fx = command::apply_fx_change(new_fx, change)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    let entry = build_control_entry(
                        next_id,
                        fx_change_src(change),
                        ControlSpec::SetFx(change),
                    );
                    next_id += 1;
                    loaded.push(entry);
                }
                Cmd::SetVol(_) => {}
                Cmd::AddPhrase {
                    source,
                    specs,
                    repeat,
                } => {
                    let resolved = resolve_rhythms(specs, &last_rhythm)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                    if let Some(r) = resolved.last() {
                        last_rhythm = r.groups.clone();
                    }
                    let phrase = build_phrase(next_id, source, resolved, 4, repeat.max(1));
                    next_id += 1;
                    loaded.push(phrase);
                }
                Cmd::Jump { to, times } => {
                    if to < 0 {
                        return Err(format!("line {line_no}: negative ids are only supported in interactive commands"));
                    }
                    let target = to as usize;
                    let entry = crate::sequencer::build_jump_entry(next_id, target, times.max(1));
                    next_id += 1;
                    loaded.push(entry);
                }
                Cmd::Clear => {
                    loaded.clear();
                    next_id = 0;
                    last_rhythm = vec![3, 3, 2];
                }
                Cmd::CreateJins { name, ratios } => {
                    crate::tuning::Maqam::create(&name, ratios)
                        .map_err(|e| format!("line {line_no}: {e}"))?;
                }
                Cmd::DeleteJins { name } => {
                    let _ = crate::tuning::Maqam::delete(&name);
                }
                _ => {
                    return Err(format!("line {line_no}: unsupported command in session"));
                }
            }
        }

        self.phrases = loaded.clone();
        self.next_phrase_id = next_id;
        self.last_rhythm = last_rhythm;
        self.bpm = new_bpm;
        self.sustain = new_sustain;
        self.vcf = new_vcf;
        self.fx = new_fx;
        self.vol = new_vol;
        self.paused = false;
        let (start_bpm, start_sustain, start_vcf, start_fx) = self.sequence_start_settings();

        let _ = self.audio_tx.send(AudioCmd::Clear);
        let _ = self.audio_tx.send(AudioCmd::SetBpm(start_bpm));
        let _ = self.audio_tx.send(AudioCmd::SetSustain(start_sustain));
        let _ = self.audio_tx.send(AudioCmd::SetVcfBank(start_vcf));
        let _ = self.audio_tx.send(AudioCmd::SetFxSettings(start_fx));
        let _ = self.audio_tx.send(AudioCmd::SetVol(self.vol));
        let _ = self.audio_tx.send(AudioCmd::SetPaused(false));
        for p in loaded {
            let _ = self.audio_tx.send(AudioCmd::AddPhrase(p));
        }
        let _ = self.audio_tx.send(AudioCmd::SetCurPhrase(0));
        Ok(())
    }
}

enum LlmRequest {
    ChatGpt {
        key: String,
        model: String,
        prompt: String,
    },
    Claude {
        key: String,
        model: String,
        prompt: String,
    },
}

impl LlmRequest {
    fn from_env(provider: LlmProvider, prompt: String) -> Option<Self> {
        match provider {
            LlmProvider::ChatGpt => Some(Self::ChatGpt {
                key: std::env::var("OPENAI_API_KEY").ok()?,
                model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
                prompt,
            }),
            LlmProvider::Claude => Some(Self::Claude {
                key: std::env::var("ANTHROPIC_API_KEY")
                    .or_else(|_| std::env::var("CLAUDE_API_KEY"))
                    .ok()?,
                model: std::env::var("ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| "claude-3-5-haiku-latest".into()),
                prompt,
            }),
        }
    }

    fn provider_name(&self) -> &'static str {
        match self {
            Self::ChatGpt { .. } => "chatgpt",
            Self::Claude { .. } => "claude",
        }
    }

    fn send(self) -> Result<String, String> {
        match self {
            Self::ChatGpt { key, model, prompt } => ask_chatgpt(&key, &model, &prompt),
            Self::Claude { key, model, prompt } => ask_claude(&key, &model, &prompt),
        }
    }
}

fn ask_chatgpt(key: &str, model: &str, prompt: &str) -> Result<String, String> {
    let response: serde_json::Value = ureq::post("https://api.openai.com/v1/chat/completions")
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .send_json(serde_json::json!({
            "model": model,
            "messages": [
                { "role": "system", "content": llm_system_prompt() },
                { "role": "user", "content": prompt }
            ],
            "temperature": 0.2
        }))
        .map_err(describe_http_error)?
        .into_json()
        .map_err(|e| format!("chatgpt response parse failed: {e}"))?;
    response
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .map(clean_llm_answer)
        .filter(|answer| !answer.is_empty())
        .ok_or_else(|| {
            "chatgpt returned no message content; try asking again, or set OPENAI_MODEL to a chat-capable model".into()
        })
}

fn ask_claude(key: &str, model: &str, prompt: &str) -> Result<String, String> {
    let response: serde_json::Value = ureq::post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", key)
        .set("anthropic-version", "2023-06-01")
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .send_json(serde_json::json!({
            "model": model,
            "max_tokens": 500,
            "system": llm_system_prompt(),
            "messages": [
                { "role": "user", "content": prompt }
            ]
        }))
        .map_err(describe_http_error)?
        .into_json()
        .map_err(|e| format!("claude response parse failed: {e}"))?;
    response
        .get("content")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(|text| text.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .map(|answer| clean_llm_answer(&answer))
        .filter(|answer| !answer.is_empty())
        .ok_or_else(|| {
            "claude returned no text content; try asking again, or set ANTHROPIC_MODEL to a messages-capable model".into()
        })
}

fn describe_http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            format!(
                "LLM HTTP {code}: {}; check the API key, model name, and account access, then try again",
                compact_error_body(&body)
            )
        }
        ureq::Error::Transport(error) => {
            format!("LLM request failed: {error}; check your network connection and try again")
        }
    }
}

fn compact_error_body(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/error/error/message"))
                .and_then(|message| message.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string());
    clean_llm_answer(&message)
}

fn clean_llm_answer(answer: &str) -> String {
    answer.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn llm_system_prompt() -> &'static str {
    "You answer concise usage questions for maqam-live, a terminal live-coding sequencer. Prefer exact commands. Useful controls: sym on/off; sym decay <0.9..0.99999> drive <0..512> amount <0..512>; sym can target all, mic, kanun, bass, drums. VCF controls: vcf [all|mic|bass|kanun|drums|sym] cut <10..22000 Hz> res <0..0.98> drive <0.1..12> wave <sin|tri|squ|saw|mic>; vcf all filters the final mix; instrument targets filter only that partition. Keep answers under three short sentences."
}

fn completion_target(input: &str) -> Option<(&str, usize, String)> {
    let trimmed = input.trim_start();
    let leading_ws = input.len().saturating_sub(trimmed.len());
    let (cmd, rest) = trimmed
        .split_once(char::is_whitespace)
        .unwrap_or((trimmed, ""));
    if cmd != "save" && cmd != "load" {
        return None;
    }
    let rest_start = leading_ws + cmd.len();
    let arg = rest.trim_start();
    let arg_start = input.len().saturating_sub(arg.len());
    Some((cmd, arg_start.max(rest_start), arg.to_string()))
}

fn phrase_completion(input: &str, phrases: &[Phrase]) -> Option<String> {
    let trimmed = input.trim_start();
    let leading_ws = input.len().saturating_sub(trimmed.len());
    let words = words_with_spans(trimmed);
    if words.len() != 1 {
        return None;
    }
    let root_token = words[0].2;
    let typed_root = crate::tuning::Pitch::parse(root_token)?;
    let current = phrases
        .iter()
        .rev()
        .find(|phrase| phrase.jump.is_none() && phrase.control.is_none())?;
    let maqam = phrase_completion_maqam(current, typed_root)?;
    let rhythm = phrase_rhythm_token(current)?;
    let mut completion = format!(
        "{}{} {} {}",
        " ".repeat(leading_ws),
        root_token,
        maqam,
        rhythm
    );
    if current.repeat > 1 {
        completion.push_str(&format!(" r{}", current.repeat));
    }
    Some(completion)
}

fn phrase_rhythm_token(phrase: &Phrase) -> Option<&str> {
    phrase
        .src
        .split_whitespace()
        .rev()
        .find(|token| token.chars().all(|ch| ch.is_ascii_digit()))
}

fn phrase_completion_maqam(
    current: &Phrase,
    typed_root: crate::tuning::Pitch,
) -> Option<&'static str> {
    let current_name = current.bar.maqam.name();
    let shift = pitch_class_delta(current.bar.root, typed_root);
    match (current_name, shift) {
        ("Bayati", 10) => Some("rast"),
        ("Minor" | "Aeolian", 3) => Some("major"),
        _ => None,
    }
}

fn pitch_class_delta(from: crate::tuning::Pitch, to: crate::tuning::Pitch) -> i8 {
    (pitch_class(to) - pitch_class(from)).rem_euclid(12)
}

fn pitch_class(pitch: crate::tuning::Pitch) -> i8 {
    let natural = match pitch.letter.to_ascii_lowercase() {
        'c' => 0,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => 0,
    };
    (natural + pitch.accidental).rem_euclid(12)
}

struct MetadataCompletion {
    replacement: Option<String>,
    message: Option<String>,
}

fn command_body_for_completion(input: &str) -> Option<(usize, &str)> {
    let trimmed = input.trim_start();
    let leading_ws = input.len().saturating_sub(trimmed.len());
    let words = words_with_spans(trimmed);
    let first = words.first()?;
    let first_text = first.2;
    let first_alpha: String = first_text
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect();
    let first_digits: String = first_text
        .chars()
        .skip_while(|ch| ch.is_ascii_alphabetic())
        .collect();
    let first_lower = first_alpha.to_ascii_lowercase();

    if first_lower == "edit" {
        let id = words.get(1)?;
        let body_start = words
            .get(2)
            .map(|word| word.0)
            .unwrap_or_else(|| trimmed.len());
        if id.2.parse::<isize>().is_err() {
            return None;
        }
        return Some((leading_ws + body_start, &trimmed[body_start..]));
    }

    if first_lower == "i" {
        if !first_digits.is_empty() {
            let body_start = words
                .get(1)
                .map(|word| word.0)
                .unwrap_or_else(|| trimmed.len());
            return Some((leading_ws + body_start, &trimmed[body_start..]));
        }
        let id = words.get(1)?;
        let body_start = words
            .get(2)
            .map(|word| word.0)
            .unwrap_or_else(|| trimmed.len());
        if id.2.parse::<isize>().is_err() {
            return None;
        }
        return Some((leading_ws + body_start, &trimmed[body_start..]));
    }

    Some((leading_ws, trimmed))
}

fn words_with_spans(input: &str) -> Vec<(usize, usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (idx, ch) in input.char_indices() {
        if ch.is_whitespace() {
            if let Some(word_start) = start.take() {
                out.push((word_start, idx, &input[word_start..idx]));
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(word_start) = start {
        out.push((word_start, input.len(), &input[word_start..]));
    }
    out
}

fn metadata_command_completion(body: &str) -> Option<MetadataCompletion> {
    let body_leading_ws = body.len().saturating_sub(body.trim_start().len());
    let body_trimmed = body.trim_start();
    if body_trimmed.is_empty() {
        return None;
    }
    let mut tokens: Vec<&str> = body_trimmed.split_whitespace().collect();
    let head = tokens.first().copied()?;
    let meta = command::command_metadata(head)?;
    let trailing_space = body_trimmed.chars().last().is_some_and(char::is_whitespace);

    if tokens.len() == 1 && !trailing_space {
        tokens.push("");
    } else if trailing_space {
        tokens.push("");
    }

    let current = tokens.last().copied().unwrap_or("");
    let before_current = &tokens[..tokens.len().saturating_sub(1)];
    if let Some(mut completion) = metadata_value_completion(meta, before_current, current) {
        if let Some(replacement) = completion.replacement {
            completion.replacement =
                Some(format!("{}{}", " ".repeat(body_leading_ws), replacement));
        }
        return Some(completion);
    }
    let replacement = metadata_command_replacement(meta, before_current, current)?;
    let replacement = format!("{}{}", " ".repeat(body_leading_ws), replacement);
    Some(MetadataCompletion {
        replacement: Some(replacement),
        message: None,
    })
}

fn metadata_command_replacement(
    meta: &'static command::CommandMetadata,
    before_current: &[&str],
    current: &str,
) -> Option<String> {
    let head = before_current.first().copied()?;
    let mut body = vec![meta.name.to_string()];
    let mut idx = 1usize;
    if let Some(target) = before_current.get(idx).and_then(|token| {
        command::command_token_name(meta.targets, canonical_completion_key(token))
    }) {
        body.push(target.to_string());
        idx += 1;
    }

    let current_key = canonical_completion_key(current);
    if idx == 1 && before_current.len() == 1 {
        if let Some(target) = exact_completion_target(meta, current_key) {
            body.push(target.to_string());
            body.push(meta.first_parameter.to_string());
            return Some(format!("{} ", body.join(" ")));
        }
        if current_key.is_empty() {
            body.push(meta.first_parameter.to_string());
            return Some(format!("{} ", body.join(" ")));
        }
        if let Some(token) = first_matching_completion_token(meta, current_key) {
            body.push(token.to_string());
            if exact_completion_target(meta, token).is_some() {
                body.push(meta.first_parameter.to_string());
            }
            return Some(format!("{} ", body.join(" ")));
        }
        return None;
    }

    let mut used = Vec::new();
    let mut scan = idx;
    while scan < before_current.len() {
        let token = before_current[scan];
        let key = canonical_completion_key(token);
        if let Some(param) = command::command_parameter(meta, key) {
            body.push(param.name.to_string());
            used.push(param.name);
            if token.contains('=') {
                scan += 1;
            } else {
                if let Some(value) = before_current.get(scan + 1) {
                    if command::command_parameter(meta, canonical_completion_key(value)).is_none()
                        && command::command_token_name(
                            meta.targets,
                            canonical_completion_key(value),
                        )
                        .is_none()
                    {
                        body.push((*value).to_string());
                        scan += 1;
                    }
                }
                scan += 1;
            }
        } else {
            body.push(token.to_string());
            scan += 1;
        }
    }

    if let Some(param) = command::command_parameter(meta, current_key) {
        body.push(param.name.to_string());
        return Some(format!("{} ", body.join(" ")));
    }
    if let Some(param) = first_matching_completion_parameter(meta, current_key, &used) {
        body.push(param.name.to_string());
        return Some(format!("{} ", body.join(" ")));
    }
    if current_key.is_empty() {
        if let Some(param) = meta
            .parameters
            .iter()
            .find(|param| !used.contains(&param.name) && param_expects_value(param))
        {
            body.push(param.name.to_string());
            return Some(format!("{} ", body.join(" ")));
        }
    }

    if head != meta.name {
        Some(body.join(" "))
    } else {
        None
    }
}

fn metadata_value_completion(
    meta: &'static command::CommandMetadata,
    before_current: &[&str],
    current: &str,
) -> Option<MetadataCompletion> {
    if before_current.len() == 2
        && command::command_token_name(meta.targets, canonical_completion_key(before_current[1]))
            .is_some()
    {
        return None;
    }
    let param = before_current
        .last()
        .and_then(|token| command::command_parameter(meta, canonical_completion_key(token)))?;
    if !param_expects_value(param) {
        return None;
    }
    if param.values.is_empty() {
        return Some(MetadataCompletion {
            replacement: None,
            message: Some(format!(
                "{} {} {}",
                meta.name,
                param.name,
                parameter_value_hint(param)
            )),
        });
    }
    let value = param
        .values
        .iter()
        .find(|value| value.starts_with(current))
        .copied()?;
    let mut body: Vec<String> = before_current
        .iter()
        .map(|token| (*token).to_string())
        .collect();
    body[0] = meta.name.to_string();
    body.push(value.to_string());
    Some(MetadataCompletion {
        replacement: Some(format!("{} ", body.join(" "))),
        message: None,
    })
}

fn param_expects_value(param: &command::CommandParameterMetadata) -> bool {
    !param.values.is_empty()
        || param.lower.is_some()
        || param.upper.is_some()
        || !param.units.is_empty()
}

fn parameter_value_hint(param: &command::CommandParameterMetadata) -> String {
    if !param.values.is_empty() {
        return format!("<{}>", param.values.join("|"));
    }
    let range = match (param.lower, param.upper) {
        (Some(lower), Some(upper)) => format!("{}..{}", compact_float(lower), compact_float(upper)),
        (Some(lower), None) => format!(">= {}", compact_float(lower)),
        (None, Some(upper)) => format!("<= {}", compact_float(upper)),
        (None, None) => "value".to_string(),
    };
    let units = if param.units.is_empty() {
        String::new()
    } else {
        format!(" {}", param.units)
    };
    format!("<{range}{units}|+n|-n|+nt>")
}

fn compact_float(value: f64) -> String {
    let mut out = format!("{value:.5}");
    while out.contains('.') && out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

fn canonical_completion_key(token: &str) -> &str {
    token.split_once('=').map(|(key, _)| key).unwrap_or(token)
}

fn exact_completion_target(
    meta: &'static command::CommandMetadata,
    token: &str,
) -> Option<&'static str> {
    if token.is_empty() {
        return None;
    }
    command::command_token_name(meta.targets, token)
}

fn first_matching_completion_token(
    meta: &'static command::CommandMetadata,
    partial: &str,
) -> Option<&'static str> {
    meta.targets
        .iter()
        .find(|target| completion_token_matches(target.name, target.aliases, partial))
        .map(|target| target.name)
        .or_else(|| first_matching_completion_parameter(meta, partial, &[]).map(|p| p.name))
}

fn first_matching_completion_parameter(
    meta: &'static command::CommandMetadata,
    partial: &str,
    used: &[&str],
) -> Option<&'static command::CommandParameterMetadata> {
    meta.parameters.iter().find(|param| {
        !used.contains(&param.name)
            && param_expects_value(param)
            && completion_token_matches(param.name, param.aliases, partial)
    })
}

fn completion_token_matches(name: &str, aliases: &[&str], partial: &str) -> bool {
    !partial.is_empty()
        && (name.starts_with(partial) || aliases.iter().any(|alias| alias.starts_with(partial)))
}

fn mq_matches(cmd: &str, partial: &str) -> Vec<String> {
    let partial_path = Path::new(partial);
    let (dir, prefix) = match partial_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => (
            PathBuf::from(parent),
            partial_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(""),
        ),
        _ => (PathBuf::from("."), partial),
    };

    let recursive = cmd == "load" && !partial.contains('/');
    let mut matches = if recursive {
        recursive_mq_matches(Path::new("."), prefix, Path::new("."))
    } else {
        direct_mq_matches(&dir, prefix)
    };
    matches.sort();
    matches.dedup();
    matches
}

fn direct_mq_matches(dir: &Path, prefix: &str) -> Vec<String> {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flat_map(|it| it.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            if entry.file_type().ok()?.is_dir() {
                return None;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("mq") {
                return None;
            }
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(prefix) {
                return None;
            }
            if dir == Path::new(".") {
                Some(name.to_string())
            } else {
                Some(dir.join(name).to_string_lossy().replace('\\', "/"))
            }
        })
        .collect()
}

fn recursive_mq_matches(dir: &Path, prefix: &str, base: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flat_map(|it| it.filter_map(Result::ok))
        .filter_map(|entry| {
            let path = entry.path();
            if entry.file_type().ok()?.is_dir() {
                return None;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("mq") {
                return None;
            }
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(prefix) {
                return None;
            }
            Some(
                path.strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        })
        .collect();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || name == "target" {
                continue;
            }
            out.extend(recursive_mq_matches(&entry.path(), prefix, base));
        }
    }
    out
}

fn longest_common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for item in &items[1..] {
        let mut keep = 0usize;
        for (a, b) in prefix.chars().zip(item.chars()) {
            if a != b {
                break;
            }
            keep += a.len_utf8();
        }
        prefix.truncate(keep);
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

fn completion_common_prefix(cmd: &str, partial: &str, matches: &[String]) -> String {
    if cmd == "load" && !partial.contains('/') {
        let basenames: Vec<String> = matches
            .iter()
            .filter_map(|path| {
                Path::new(path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .collect();
        return longest_common_prefix(&basenames);
    }
    longest_common_prefix(matches)
}

fn resolve_rhythms(specs: Vec<JinsSpec>, default: &[u8]) -> Result<Vec<BarSpec>, String> {
    let n = specs.len();
    let mut groups: Vec<Option<Vec<u8>>> = specs.iter().map(|s| s.groups.clone()).collect();
    let mut carry: Option<Vec<u8>> = None;
    for i in (0..n).rev() {
        if groups[i].is_some() {
            carry = groups[i].clone();
        } else {
            groups[i] = carry.clone();
        }
    }
    let fallback = default.to_vec();
    Ok(specs
        .into_iter()
        .zip(groups)
        .map(|(spec, grp)| BarSpec {
            src: spec.src,
            root: spec.root,
            maqam: spec.maqam,
            groups: grp.unwrap_or_else(|| fallback.clone()),
        })
        .collect())
}

fn apply_bpm_change(current: f64, change: ValueChange) -> Result<f64, String> {
    let next = change.apply(current)?;
    if !(20.0..=400.0).contains(&next) {
        return Err(format!("bpm {next} out of range"));
    }
    Ok(next)
}

fn apply_sustain_change(current: f64, change: ValueChange) -> Result<f64, String> {
    let next = change.apply(current)?;
    if !(0.05..=10.0).contains(&next) {
        return Err(format!("sustain {next}s out of range"));
    }
    Ok(next)
}

fn vcf_change_src(change: command::VcfChange) -> String {
    let mut parts = vec!["vcf".to_string()];
    if let Some(target) = change.target {
        parts.push(target.as_str().to_string());
    }
    if change.enabled == Some(false) {
        if change.target == Some(VcfTarget::All) || change.target.is_none() {
            return "vcf off".to_string();
        }
        return format!("vcf {} off", change.target.unwrap().as_str());
    }
    if let Some(cutoff) = change.cutoff_hz {
        parts.push("cut".to_string());
        parts.push(value_change_src(cutoff));
    }
    if let Some(resonance) = change.resonance {
        parts.push("res".to_string());
        parts.push(value_change_src(resonance));
    }
    if let Some(drive) = change.drive {
        parts.push("drive".to_string());
        parts.push(value_change_src(drive));
    }
    if let Some(wave) = change
        .wave
        .filter(|_| !matches!(change.target, Some(VcfTarget::All | VcfTarget::Mic)))
    {
        parts.push("wave".to_string());
        parts.push(wave.as_str().to_string());
    }
    parts.join(" ")
}

fn sym_change_src(change: command::SympatheticChange) -> String {
    let mut parts = vec!["sym".to_string()];
    if let Some(target) = change.target {
        parts.push(target.as_str().to_string());
    }
    if let Some(enabled) = change.enabled {
        parts.push(if enabled { "on" } else { "off" }.to_string());
    }
    if let Some(decay) = change.decay {
        parts.push("decay".to_string());
        parts.push(format!("{decay}"));
    }
    if let Some(gain) = change.gain {
        parts.push("drive".to_string());
        parts.push(format!("{gain}"));
    }
    if let Some(amount) = change.amount {
        parts.push("amount".to_string());
        parts.push(format!("{amount}"));
    }
    if let Some(mic) = change.mic {
        parts.push("mic".to_string());
        parts.push(format!("{mic}"));
    }
    if let Some(kanun) = change.kanun {
        parts.push("kanun".to_string());
        parts.push(format!("{kanun}"));
    }
    if let Some(bass) = change.bass {
        parts.push("bass".to_string());
        parts.push(format!("{bass}"));
    }
    if let Some(drums) = change.drums {
        parts.push("drums".to_string());
        parts.push(format!("{drums}"));
    }
    parts.join(" ")
}

fn value_change_src(change: ValueChange) -> String {
    match change {
        ValueChange::Set(n) => format!("{n}"),
        ValueChange::Add(n) if n < 0.0 => format!("{n}"),
        ValueChange::Add(n) => format!("+{n}"),
        ValueChange::Mul(n) => format!("*{n}"),
        ValueChange::Div(n) => format!("/{n}"),
        ValueChange::Tick(n) if n < 0.0 => format!("{n}t"),
        ValueChange::Tick(n) => format!("+{n}t"),
    }
}

fn fx_change_src(change: command::FxChange) -> String {
    if change.reverb_enabled == Some(false) && change.delay_enabled == Some(false) {
        return "fx off".to_string();
    }
    let mut parts = Vec::new();
    if change.reverb_enabled.is_some()
        || change.reverb_mix.is_some()
        || change.reverb_decay.is_some()
    {
        parts.push("reverb".to_string());
        if change.reverb_enabled == Some(false) {
            parts.push("off".to_string());
            return parts.join(" ");
        }
        if let Some(mix) = change.reverb_mix {
            parts.push("mix".to_string());
            parts.push(value_change_src(mix));
        }
        if let Some(decay) = change.reverb_decay {
            parts.push("decay".to_string());
            parts.push(value_change_src(decay));
        }
    } else {
        parts.push("delay".to_string());
        if change.delay_enabled == Some(false) {
            parts.push("off".to_string());
            return parts.join(" ");
        }
        if let Some(time) = change.delay_time_secs {
            parts.push("time".to_string());
            parts.push(value_change_src(time));
        }
        if let Some(feedback) = change.delay_feedback {
            parts.push("feedback".to_string());
            parts.push(value_change_src(feedback));
        }
        if let Some(mix) = change.delay_mix {
            parts.push("mix".to_string());
            parts.push(value_change_src(mix));
        }
    }
    parts.join(" ")
}

fn describe_fx(fx: FxSettings) -> String {
    let rev = if fx.reverb_enabled {
        format!("rev {:.2}/{:.2}", fx.reverb_mix, fx.reverb_decay)
    } else {
        "rev off".to_string()
    };
    let delay = if fx.delay_enabled {
        format!(
            "delay {:.2}s/{:.2}/{:.2}",
            fx.delay_time_secs, fx.delay_feedback, fx.delay_mix
        )
    } else {
        "delay off".to_string()
    };
    format!("{rev} {delay}")
}

fn describe_vcf(v: VcfSettings) -> String {
    if !v.enabled {
        if v.target == VcfTarget::All {
            return "vcf off".to_string();
        }
        return format!("vcf {} off", v.target.as_str());
    }
    format!(
        "vcf {} cut {:.1} Hz  res {:.2}  drive {:.2}  {}",
        v.target.as_str(),
        v.cutoff_hz,
        v.resonance,
        v.drive,
        v.wave.as_str()
    )
}

fn is_plain_control_line(line: &str) -> bool {
    line.starts_with("bpm ")
        || line.starts_with("s ")
        || line.starts_with("sus ")
        || is_plain_vcf_control_line(line)
        || is_plain_fx_control_line(line)
        || line
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("sym"))
}

fn is_plain_vcf_control_line(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    matches!(
        first.to_ascii_lowercase().as_str(),
        "vcf" | "filter" | "filt" | "cut" | "cutoff" | "res" | "q" | "drive" | "drv"
    )
}

fn is_plain_fx_control_line(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    matches!(
        first.to_ascii_lowercase().as_str(),
        "fx" | "reverb" | "rev" | "delay" | "pingpong"
    )
}

fn resolve_id_ref_in_phrases(phrases: &[Phrase], id_ref: isize) -> Option<usize> {
    if id_ref == crate::command::START_REF {
        return phrases.first().map(|phrase| phrase.id);
    }
    if id_ref >= 0 {
        return Some(id_ref as usize);
    }
    let back = id_ref.unsigned_abs();
    if back == 0 || back > phrases.len() {
        return None;
    }
    phrases.get(phrases.len() - back).map(|p| p.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn session_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn phrase_display_source_preserves_the_users_text() {
        let (tx, _rx) = bounded(8);
        let mut app = App::new(tx);

        app.handle_command("d nah 332,   a hij 44 r3");

        assert_eq!(app.phrases[0].src, "d nah 332,   a hij 44 r3");
        assert_eq!(app.phrases[0].repeat, 3);
    }

    #[test]
    fn western_mode_names_parse_as_builtin_jins() {
        let (tx, _rx) = bounded(8);
        let mut app = App::new(tx);

        app.handle_command("d major 332");
        app.handle_command("e dorian 332");
        app.handle_command("f locrian 332");
        app.handle_command("g diminished 332");

        assert_eq!(app.phrases[0].bar.maqam_names, vec!["Major"]);
        assert_eq!(
            app.phrases[0].bar.ratio_strs[0],
            "1/1 9/8 5/4 4/3 3/2 5/3 15/8"
        );
        assert_eq!(app.phrases[1].bar.maqam_names, vec!["Dorian"]);
        assert_eq!(app.phrases[2].bar.maqam_names, vec!["Locrian"]);
        assert_eq!(
            app.phrases[2].bar.ratio_strs[0],
            "1/1 16/15 6/5 4/3 64/45 8/5 9/5"
        );
        assert_eq!(app.phrases[3].bar.maqam_names, vec!["Diminished"]);
        assert_eq!(
            app.phrases[3].bar.ratio_strs[0],
            "1/1 9/8 6/5 4/3 64/45 8/5 5/3 15/8"
        );
    }

    #[test]
    fn inserts_sym_drive_as_a_timeline_control() {
        let (tx, _rx) = bounded(32);
        let mut app = App::new(tx);
        for _ in 0..5 {
            app.handle_command("d bayati 4");
        }

        app.handle_command("i 4 sym drive 64");

        assert_eq!(app.phrases[4].id, 5);
        assert_eq!(app.phrases[4].src, "sym drive 64");
        assert!(matches!(
            app.phrases[4].control,
            Some(ControlSpec::SetSympatheticGain(64.0))
        ));
        assert_eq!(app.phrases[5].id, 4);

        app.handle_command("edit 5 sym gain 96");
        assert_eq!(app.phrases[4].id, 5);
        assert_eq!(app.phrases[4].src, "sym gain 96");
        assert!(matches!(
            app.phrases[4].control,
            Some(ControlSpec::SetSympatheticGain(96.0))
        ));

        app.handle_command("edit 5 sym decay 0.999 drive 2 kanun 0.5 bass 0.5");
        assert_eq!(
            app.phrases[4].src,
            "sym decay 0.999 drive 2 kanun 0.5 bass 0.5"
        );
        let Some(ControlSpec::SetSympathetic(change)) = app.phrases[4].control else {
            panic!("expected combined sym control");
        };
        assert_eq!(change.target, None);
        assert_eq!(change.enabled, None);
        assert_eq!(change.decay, Some(0.999));
        assert_eq!(change.gain, Some(2.0));
        assert_eq!(change.amount, None);
        assert_eq!(change.mic, None);
        assert_eq!(change.kanun, Some(0.5));
        assert_eq!(change.bass, Some(0.5));
        assert_eq!(change.drums, None);

        app.handle_command("edit 5 sym mic decay 0.9999 drive 8 amount 1.5");
        assert_eq!(
            app.phrases[4].src,
            "sym mic decay 0.9999 drive 8 amount 1.5"
        );
        let Some(ControlSpec::SetSympathetic(change)) = app.phrases[4].control else {
            panic!("expected targeted sym control");
        };
        assert_eq!(change.target, Some(command::SympatheticTarget::Mic));
        assert_eq!(change.enabled, None);
        assert_eq!(change.decay, Some(0.9999));
        assert_eq!(change.gain, Some(8.0));
        assert_eq!(change.amount, Some(1.5));
        assert_eq!(change.kanun, None);
    }

    #[test]
    fn loads_and_saves_v3_without_rewriting_input() {
        let _guard = session_test_lock();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let input_path = std::env::temp_dir().join(format!("maqam-v3-input-{suffix}.mq"));
        let output_path = std::env::temp_dir().join(format!("maqam-v3-output-{suffix}.mq"));
        let source = concat!(
            "MAQAM_SESSION_V3\n",
            "create testv3 1/1 9/8 5/4\n",
            "vol 0.75\n",
            "B|4|180\n",
            "S|7|1.2\n",
            "Y|8|sym on\n",
            "Y|9|sym gain 64\n",
            "Y|10|sym decay 0.99\n",
            "P|11|2|g testv3 332\n",
            "J|15|11|3\n",
        );
        fs::write(&input_path, source).unwrap();

        let (tx, _rx) = bounded(32);
        let mut app = App::new(tx);
        app.vol = 0.42;
        app.load_session(input_path.to_str().unwrap()).unwrap();
        assert_eq!(app.vol, 0.42);

        assert_eq!(fs::read_to_string(&input_path).unwrap(), source);
        assert_eq!(
            app.phrases
                .iter()
                .map(|phrase| phrase.id)
                .collect::<Vec<_>>(),
            vec![4, 7, 8, 9, 10, 11, 15]
        );
        assert!(matches!(
            app.phrases[2].control,
            Some(ControlSpec::SetSympathetics(true))
        ));
        assert!(matches!(
            app.phrases[3].control,
            Some(ControlSpec::SetSympatheticGain(64.0))
        ));
        assert!(matches!(
            app.phrases[4].control,
            Some(ControlSpec::SetSympatheticDecay(0.99))
        ));
        assert_eq!(app.phrases[5].repeat, 2);
        assert_eq!(app.phrases[6].jump.as_ref().unwrap().target_id, 11);
        assert_eq!(app.next_phrase_id, 16);

        app.save_session(output_path.to_str().unwrap()).unwrap();
        let saved = fs::read_to_string(&output_path).unwrap();
        assert!(saved.starts_with("MAQAM_SESSION_V3\n"));
        assert!(!saved.contains("\nvol "));
        assert!(saved.contains("B|4|180\n"));
        assert!(saved.contains("Y|8|sym on\n"));
        assert!(saved.contains("Y|9|sym gain 64\n"));
        assert!(saved.contains("Y|10|sym decay 0.99\n"));
        assert!(saved.contains("P|11|2|g testv3 332\n"));

        let _ = fs::remove_file(input_path);
        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn loads_legacy_control_lines_under_v3_header() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);
        app.load_session_v3(["bpm 180", "s 1.2", "P|2|1|g hijaz 4444"].into_iter())
            .unwrap();

        assert_eq!(
            app.phrases
                .iter()
                .map(|phrase| phrase.id)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(app.bpm, 180.0);
        assert_eq!(app.sustain, 1.2);
    }

    #[test]
    fn loads_vcf_control_lines_under_v3_header() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);
        app.load_session_v3(
            [
                "vcf bass cut=900 res=0.65 drive=3.5",
                "cut +100",
                "V|5|kanun|1200|0.4|2.25",
                "P|6|1|g hijaz 4444",
            ]
            .into_iter(),
        )
        .unwrap();

        assert_eq!(
            app.phrases
                .iter()
                .map(|phrase| phrase.id)
                .collect::<Vec<_>>(),
            vec![0, 1, 5, 6]
        );
        assert_eq!(app.vcf.kanun.cutoff_hz, 1200.0);
        assert_eq!(app.vcf.kanun.resonance, 0.4);
        assert_eq!(app.vcf.kanun.drive, 2.25);
        assert_eq!(app.vcf.kanun.target, VcfTarget::Kanun);
    }

    #[test]
    fn vcf_off_is_a_transparent_control_command() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);

        app.handle_command("vcf");
        assert!(app.vcf.all.enabled);
        assert_eq!(app.vcf.all.target, VcfTarget::All);
        assert_eq!(app.phrases.last().unwrap().src, "vcf");

        app.handle_command("vcf off");
        assert!(!app.vcf.all.enabled);
        assert!(!app.vcf.mic.enabled);
        assert!(!app.vcf.bass.enabled);
        assert!(!app.vcf.kanun.enabled);
        assert!(!app.vcf.kick.enabled);
        assert!(!app.vcf.tanbura.enabled);
        assert_eq!(app.vcf.focus, VcfTarget::All);

        app.handle_command("vcf bass off");
        assert!(!app.vcf.all.enabled);
        assert!(!app.vcf.mic.enabled);
        assert!(!app.vcf.bass.enabled);
        assert!(!app.vcf.kanun.enabled);
        assert!(!app.vcf.kick.enabled);
        assert!(!app.vcf.tanbura.enabled);
        assert_eq!(app.vcf.focus, VcfTarget::Bass);

        app.handle_command("vcf bass 900 0.65 3.5");
        assert!(app.vcf.bass.enabled);
        assert_eq!(app.vcf.bass.target, VcfTarget::Bass);
    }

    #[test]
    fn vcf_wave_is_named() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(64);
        let mut app = App::new(tx);

        app.handle_command("vcf bass 900 0.65 3.5 wave=saw");
        assert!(app.vcf.bass.enabled);
        assert_eq!(app.vcf.bass.target, VcfTarget::Bass);
        assert_eq!(app.vcf.bass.cutoff_hz, 900.0);
        assert_eq!(app.vcf.bass.resonance, 0.65);
        assert_eq!(app.vcf.bass.drive, 3.5);
        assert_eq!(app.vcf.bass.wave, VcoWave::Saw);

        app.handle_command("vcf kanun cut=2400 res=0.35 drive=2.0 wave=tri");
        assert!(app.vcf.bass.enabled);
        assert!(app.vcf.kanun.enabled);
        assert!(!app.vcf.all.enabled);
        assert_eq!(app.vcf.kanun.target, VcfTarget::Kanun);
        assert_eq!(app.vcf.kanun.wave, VcoWave::Tri);

        app.handle_command("vcf drums cut=700 res=0.25 drive=2.5 wave=squ");
        assert!(app.vcf.bass.enabled);
        assert!(app.vcf.kanun.enabled);
        assert!(app.vcf.kick.enabled);
        assert_eq!(app.vcf.kick.target, VcfTarget::Kick);
        assert_eq!(app.vcf.kick.wave, VcoWave::Squ);
        assert_eq!(
            app.phrases.last().unwrap().src,
            "vcf drums cut 700 res 0.25 drive 2.5 wave squ"
        );

        app.handle_command("vcf mic cut=1800 res=0.2 drive=1.2 wave=sin");
        assert!(app.vcf.mic.enabled);
        assert_eq!(app.vcf.mic.target, VcfTarget::Mic);
        assert_eq!(app.vcf.mic.cutoff_hz, 1800.0);
        assert_eq!(app.vcf.mic.wave, VcoWave::Mic);

        app.handle_command("vcf mic cut 1200 res 0.6 drive 2 wave mic");
        assert!(app.vcf.mic.enabled);
        assert_eq!(app.vcf.mic.cutoff_hz, 1200.0);
        assert_eq!(app.vcf.mic.resonance, 0.6);
        assert_eq!(app.vcf.mic.drive, 2.0);
        assert_eq!(app.vcf.mic.wave, VcoWave::Mic);
        assert_eq!(
            app.phrases.last().unwrap().src,
            "vcf mic cut 1200 res 0.6 drive 2"
        );

        app.handle_command("vcf sym cut=1800 res=0.7 drive=1.5");
        assert!(app.vcf.tanbura.enabled);
        assert_eq!(app.vcf.tanbura.target, VcfTarget::Tanbura);
        assert_eq!(app.vcf.tanbura.cutoff_hz, 1800.0);
        assert_eq!(
            app.phrases.last().unwrap().src,
            "vcf sym cut 1800 res 0.7 drive 1.5"
        );

        app.handle_command("vcf bass off");
        assert!(!app.vcf.bass.enabled);
        assert!(app.vcf.kanun.enabled);
        assert!(app.vcf.kick.enabled);
        assert_eq!(app.vcf.focus, VcfTarget::Bass);

        app.handle_command("vcf all 1200 0.35 1.5 wave=saw");
        assert!(app.vcf.all.enabled);
        assert_eq!(app.vcf.all.wave, VcoWave::Sin);
        assert!(!app.vcf.mic.enabled);
        assert!(!app.vcf.bass.enabled);
        assert!(!app.vcf.kanun.enabled);
        assert!(!app.vcf.kick.enabled);
        assert!(!app.vcf.tanbura.enabled);
        assert_eq!(
            app.phrases.last().unwrap().src,
            "vcf all cut 1200 res 0.35 drive 1.5"
        );

        app.handle_command("vcf all off");
        assert!(!app.vcf.all.enabled);
        assert!(!app.vcf.mic.enabled);
        assert!(!app.vcf.bass.enabled);
        assert!(!app.vcf.kanun.enabled);
        assert!(!app.vcf.kick.enabled);
        assert!(!app.vcf.tanbura.enabled);
        assert_eq!(app.vcf.focus, VcfTarget::All);
    }

    #[test]
    fn vcf_relative_and_tick_changes_are_preserved() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);

        app.handle_command("vcf bass 900 0.65 3.5 wave=saw");
        app.handle_command("vcf bass cut -100");
        assert_eq!(app.vcf.bass.cutoff_hz, 800.0);
        assert_eq!(app.phrases.last().unwrap().src, "vcf bass cut -100");

        app.handle_command("vcf bass cut=+2t");
        assert_eq!(app.vcf.bass.cutoff_step_per_tick, 2.0);
        assert_eq!(app.phrases.last().unwrap().src, "vcf bass cut +2t");

        app.handle_command("vcf bass cut=+0");
        assert_eq!(app.vcf.bass.cutoff_step_per_tick, 0.0);
        assert_eq!(app.vcf.bass.cutoff_hz, 800.0);
    }

    #[test]
    fn fx_commands_use_vcf_style_parameter_rules() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);

        app.handle_command("reverb mix=0.25 decay=0.7");
        assert!(app.fx.reverb_enabled);
        assert_eq!(app.fx.reverb_mix, 0.25);
        assert_eq!(app.fx.reverb_decay, 0.7);

        app.handle_command("pingpong time=0.33 feedback=0.45 mix=0.2");
        assert!(app.fx.delay_enabled);
        assert_eq!(app.fx.delay_time_secs, 0.33);
        assert_eq!(app.fx.delay_feedback, 0.45);
        assert_eq!(app.fx.delay_mix, 0.2);

        app.handle_command("delay mix=+0.1");
        assert_eq!(app.fx.delay_mix, 0.3);
        assert_eq!(app.phrases.last().unwrap().src, "delay mix +0.1");

        app.handle_command("delay feedback=+0.01t");
        assert_eq!(app.fx.delay_feedback_step_per_tick, 0.01);
        assert_eq!(app.phrases.last().unwrap().src, "delay feedback +0.01t");

        app.handle_command("fx off");
        assert!(!app.fx.reverb_enabled);
        assert!(!app.fx.delay_enabled);
    }

    #[test]
    fn load_tab_completion_lists_and_completes_mq_files() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("maqam-complete-{suffix}"));
        fs::create_dir(&root).unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        struct CwdGuard(PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(old_cwd);
        fs::write("alpha.mq", "MAQAM_SESSION_V3\n").unwrap();
        fs::write("alpine.mq", "MAQAM_SESSION_V3\n").unwrap();
        fs::create_dir("sets").unwrap();
        fs::write("sets/alphaDeep.mq", "MAQAM_SESSION_V3\n").unwrap();

        app.input = "load al".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "load alp");
        assert_eq!(
            app.message.as_deref(),
            Some("load: alpha.mq  alpine.mq  sets/alphaDeep.mq")
        );

        app.complete_input();
        assert_eq!(app.input, "load alp");
        assert_eq!(
            app.message.as_deref(),
            Some("load: alpha.mq  alpine.mq  sets/alphaDeep.mq")
        );

        app.input = "load alphaD".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "load sets/alphaDeep.mq");
        assert!(app.message.is_none());
    }

    #[test]
    fn edit_tab_completion_fills_current_timeline_value() {
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);

        app.handle_command("d bayati 332 r3");
        app.input = "edit 0".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "edit 0 d bayati 332 r3");
        assert!(app.message.is_none());

        app.handle_command("vcf bass cut=900 res=0.65");
        app.input = "edit 1 ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "edit 1 vcf bass cut 900 res 0.65");
        assert!(app.message.is_none());
    }

    #[test]
    fn command_metadata_drives_parameter_completion() {
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);

        app.input = "vcf".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "vcf cut ");
        assert!(app.message.is_none());

        app.input = "vcf mic ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "vcf mic cut ");

        app.input = "vcf mic cut ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "vcf mic cut ");
        assert_eq!(
            app.message.as_deref(),
            Some("vcf cut <10..22000 Hz|+n|-n|+nt>")
        );

        app.input = "vcf bass wave s".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "vcf bass wave sin ");
        assert!(app.message.is_none());

        app.input = "i 4 vcf bass ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "i 4 vcf bass cut ");

        app.input = "edit 4 sym mic ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "edit 4 sym mic decay ");
    }

    #[test]
    fn llm_missing_key_error_tells_user_what_to_do() {
        let _guard = session_test_lock();
        let old_key = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");

        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);
        app.handle_command("chatgpt: what is a jins?");

        assert_eq!(
            app.message.as_deref(),
            Some("✗ environment variable OPENAI_API_KEY needs to be set to talk to chatgpt")
        );

        if let Some(key) = old_key {
            std::env::set_var("OPENAI_API_KEY", key);
        }
    }

    #[test]
    fn phrase_completion_uses_current_phrase_transition_rules() {
        let (tx, _rx) = bounded(16);
        let mut app = App::new(tx);

        app.handle_command("d bayati 4444");
        app.input = "c ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "c rast 4444");

        app.handle_command("e minor 332 r2");
        app.input = "g ".to_string();
        app.cursor_pos = app.input.chars().count();
        app.complete_input();
        assert_eq!(app.input, "g major 332 r2");
    }

    #[test]
    fn loads_rewritten_v1_session_with_custom_jins() {
        let _guard = session_test_lock();
        let (tx, _rx) = bounded(32);
        let mut app = App::new(tx);
        app.load_session_v1(
            [
                "create saba2 1/1 13/12 6/5 5/4",
                "vol 1",
                "bpm 180",
                "s 2",
                "P|2|1|d bayati, f hijaz 4444",
                "J|3|0|3",
                "P|4|1|a saba, c hijaz",
                "P|5|1|a saba2, c hijaz",
                "J|6|4|4",
                "P|7|1|g rast 664664",
                "J|8|7|4",
            ]
            .into_iter(),
        )
        .unwrap();

        assert_eq!(app.phrases.len(), 9);
        assert_eq!(app.next_phrase_id, 9);
    }

    #[test]
    fn bundled_v3_sessions_load() {
        let _guard = session_test_lock();

        for name in ["magiccarpet.mq", "growl.mq"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
            let source = fs::read_to_string(&path).unwrap();
            assert!(source.starts_with("MAQAM_SESSION_V3\n"), "{name} is not V3");

            let (tx, _rx) = bounded(32);
            let mut app = App::new(tx);
            app.load_session(path.to_str().unwrap())
                .unwrap_or_else(|error| panic!("{name} failed to load: {error}"));

            assert!(!app.phrases.is_empty(), "{name} loaded no timeline entries");
            assert!(app
                .phrases
                .iter()
                .all(|phrase| phrase.id < app.next_phrase_id));
        }
    }

    #[test]
    fn recording_errors_appear_in_response_area() {
        let (audio_tx, _audio_rx) = bounded(1);
        let mut app = App::new(audio_tx);
        let (result_tx, result_rx) = bounded(1);
        app.rec_rx = Some(result_rx);
        result_tx
            .send(Err("generated source background failed".to_string()))
            .unwrap();

        app.tick();

        assert_eq!(
            app.message.as_deref(),
            Some("✗ generated source background failed")
        );
        assert!(app.rec_rx.is_none());
    }

    #[test]
    #[ignore]
    fn offline_carpet_video_smoke_test() {
        let _guard = session_test_lock();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("magiccarpet.mq");
        let (tx, _rx) = bounded(32);
        let mut app = App::new(tx);
        app.load_session(path.to_str().unwrap()).unwrap();
        let (bpm, sustain, vcf, fx) = app.sequence_start_settings();
        let output =
            crate::record::record_cycle(app.phrases.clone(), bpm, sustain, vcf, fx, 1).unwrap();
        assert!(Path::new(&output).exists());
        let _ = fs::remove_file(output);
    }
}
