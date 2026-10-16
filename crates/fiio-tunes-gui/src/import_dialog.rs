//! The Import… dialog, in the style of the Selection dialog: a source
//! (iprune's playlists folder, the iPod connected or an `iTunesDB` file),
//! then its playlists to check, what to do with a name already taken, and
//! for an iPod whether its iprune scope becomes the X3's. Reading and
//! importing run on workers.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::Rc;

use fiio_core::import::OnConflict;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::import_job::{self, Listing, Loaded, Source};
use crate::{Ui, joined, result_page, working_page};

/// The conflict choices, in the drop-down's order.
pub const CONFLICTS: [(OnConflict, &str); 3] = [
    (OnConflict::Skip, "Skip it, keep fiio-tunes' playlist"),
    (OnConflict::Replace, "Replace fiio-tunes' playlist"),
    (OnConflict::Rename, "Import it under a new name"),
];

/// The names to pass to the import: an empty list (meaning all) when every
/// playlist offered is checked, so two of one name on an iPod both come;
/// otherwise those checked, each once. `None` when none is checked.
pub fn names_to_import(offered: &[String], checked: &BTreeSet<String>) -> Option<Vec<String>> {
    let picked: BTreeSet<&String> = offered.iter().filter(|n| checked.contains(*n)).collect();
    if picked.is_empty() {
        return None;
    }
    Some(match offered.iter().all(|n| checked.contains(n)) {
        true => Vec::new(),
        false => picked.into_iter().cloned().collect(),
    })
}

fn page() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_top(16);
    page.set_margin_bottom(16);
    page.set_margin_start(16);
    page.set_margin_end(16);
    page
}

fn heading(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("title-2");
    l.set_xalign(0.0);
    l
}

impl Ui {
    /// Opens the Import… dialog on its first page, the source.
    pub(crate) fn open_import(self: &Rc<Self>) {
        let dialog = gtk::Window::builder()
            .title("Import Playlists")
            .transient_for(&self.window)
            .modal(true)
            .default_width(560)
            .default_height(620)
            .build();
        {
            // The window manager's close key too is refused while playlists
            // are written.
            let ui = Rc::downgrade(self);
            dialog.connect_close_request(move |_| match ui.upgrade() {
                Some(ui) if ui.writing.get() => glib::Propagation::Stop,
                _ => glib::Propagation::Proceed,
            });
        }
        let stack = gtk::Stack::new();
        dialog.set_child(Some(&stack));

        let body = page();
        body.append(&heading("Import playlists from"));
        let iprune = gtk::CheckButton::with_label("iprune's playlists folder");
        let ipod = gtk::CheckButton::with_label("The iPod connected");
        let file = gtk::CheckButton::with_label("An iTunesDB file (a copy of an iPod's database)");
        ipod.set_group(Some(&iprune));
        file.set_group(Some(&iprune));
        iprune.set_active(true);
        let chosen: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let file_label = gtk::Label::new(Some("No file chosen"));
        file_label.set_xalign(0.0);
        file_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        file_label.add_css_class("dim-label");
        file_label.set_hexpand(true);
        let choose = gtk::Button::with_label("Choose…");
        let file_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        file_row.set_margin_start(28);
        file_row.append(&file_label);
        file_row.append(&choose);
        let note = gtk::Label::new(Some(
            "Nothing is written to the iPod, its backups or iprune's files. Playlists are \
             written to fiio-tunes' playlists folder.",
        ));
        note.set_wrap(true);
        note.set_xalign(0.0);
        note.add_css_class("dim-label");
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        let cancel = gtk::Button::with_mnemonic("_Cancel");
        let next = gtk::Button::with_mnemonic("_Next");
        next.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&next);
        for w in [
            iprune.upcast_ref::<gtk::Widget>(),
            ipod.upcast_ref(),
            file.upcast_ref(),
            file_row.upcast_ref(),
            note.upcast_ref(),
            spacer.upcast_ref(),
            buttons.upcast_ref(),
        ] {
            body.append(w);
        }
        crate::closes(&cancel, &dialog);

        // Next needs a file when the file is the source.
        let update = {
            let (file, chosen, next, file_row) =
                (file.clone(), chosen.clone(), next.clone(), file_row.clone());
            Rc::new(move || {
                file_row.set_sensitive(file.is_active());
                next.set_sensitive(!file.is_active() || chosen.borrow().is_some());
            })
        };
        for radio in [&iprune, &ipod, &file] {
            let update = update.clone();
            radio.connect_toggled(move |_| update());
        }
        update();
        {
            let (dialog_weak, chosen, label, update) = (
                dialog.downgrade(),
                chosen.clone(),
                file_label.clone(),
                update.clone(),
            );
            choose.connect_clicked(move |_| {
                let picker = gtk::FileDialog::builder()
                    .title("Choose an iTunesDB file")
                    .modal(true)
                    .build();
                let (chosen, label, update) = (chosen.clone(), label.clone(), update.clone());
                picker.open(
                    dialog_weak.upgrade().as_ref(),
                    gio::Cancellable::NONE,
                    move |r| {
                        if let Some(path) = r.ok().and_then(|f| f.path()) {
                            label.set_text(&path.display().to_string());
                            *chosen.borrow_mut() = Some(path);
                            update();
                        }
                    },
                );
            });
        }
        {
            let ui = Rc::downgrade(self);
            let (dialog_weak, stack) = (dialog.downgrade(), stack.clone());
            next.connect_clicked(move |_| {
                let (Some(ui), Some(dialog)) = (ui.upgrade(), dialog_weak.upgrade()) else {
                    return;
                };
                let source = if iprune.is_active() {
                    Source::Iprune
                } else if ipod.is_active() {
                    Source::Ipod
                } else {
                    match chosen.borrow().clone() {
                        Some(f) => Source::File(f),
                        None => return,
                    }
                };
                ui.load_import(&dialog, &stack, source);
            });
        }
        stack.add_named(&body, Some("source"));
        stack.set_visible_child(&body);
        dialog.present();
    }

    /// Reads the source on a worker, then shows its playlists.
    fn load_import(self: &Rc<Self>, dialog: &gtk::Window, stack: &gtk::Stack, source: Source) {
        working_page(
            stack,
            match source {
                Source::Iprune => "Reading iprune's playlists…",
                _ => "Reading the iPod's database and tracing its tracks to the library…",
            },
        );
        let progress = gtk::Label::new(None);
        if let Some(page) = stack.visible_child().and_downcast::<gtk::Box>() {
            page.append(&progress);
        }
        let (tx, rx) = async_channel::unbounded::<(usize, usize)>();
        glib::spawn_future_local(async move {
            while let Ok((done, total)) = rx.recv().await {
                progress.set_text(&format!(
                    "Comparing iPod files with the library: {done} of {total}"
                ));
            }
        });
        let config = self.data.config();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let (dialog, stack) = (dialog.downgrade(), stack.clone());
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let progress = |done: usize, total: usize| {
                    let _ = tx.send_blocking((done, total));
                };
                import_job::load(&config, &paths, &source, &progress)
            })
            .await;
            let (Some(ui), Some(dialog)) = (ui.upgrade(), dialog.upgrade()) else {
                return;
            };
            match joined(result) {
                Ok(listing) => ui.choose_import(&dialog, &stack, listing),
                Err(e) => result_page(
                    &dialog,
                    &stack,
                    "The playlists could not be read",
                    &format!("{e:#}\n\nNothing was imported."),
                ),
            }
        });
    }

    /// The second page: the playlists to check, the conflict choice and,
    /// for an iPod, the scope.
    fn choose_import(self: &Rc<Self>, dialog: &gtk::Window, stack: &gtk::Stack, listing: Listing) {
        let body = page();
        body.append(&heading("Playlists to import"));
        let about = gtk::Label::new(Some(&listing.about));
        about.set_xalign(0.0);
        about.set_wrap(true);
        about.add_css_class("dim-label");
        body.append(&about);

        let offered: Vec<String> = listing.offers.iter().map(|o| o.name.clone()).collect();
        let checked: Rc<RefCell<BTreeSet<String>>> =
            Rc::new(RefCell::new(offered.iter().cloned().collect()));
        let filter = gtk::SearchEntry::new();
        filter.set_placeholder_text(Some("Filter playlists"));
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        let mut checks = Vec::new();
        for o in &listing.offers {
            let mut label = format!("{}  ({})", o.name, o.detail);
            if o.taken {
                label.push_str(", name taken");
            }
            let check = gtk::CheckButton::with_label(&label);
            check.set_active(true);
            check.set_margin_start(6);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&check));
            row.set_activatable(false);
            row.set_widget_name(&o.name);
            list.append(&row);
            checks.push(check);
        }
        if listing.offers.is_empty() {
            let none = gtk::Label::new(Some("The source has no playlist."));
            none.add_css_class("dim-label");
            list.set_placeholder(Some(&none));
        }
        {
            let filter = filter.clone();
            list.set_filter_func(move |row| {
                crate::view_model::Query::new(&filter.text()).matches_name(&row.widget_name())
            });
        }
        {
            let list = list.downgrade();
            filter.connect_search_changed(move |_| {
                if let Some(list) = list.upgrade() {
                    list.invalidate_filter();
                }
            });
        }
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroll.add_css_class("frame");
        let all = gtk::Button::with_label("Check All");
        let none = gtk::Button::with_label("Check None");
        let pick = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        pick.append(&all);
        pick.append(&none);

        let conflict_label = gtk::Label::new(Some("When fiio-tunes has a playlist of that name:"));
        conflict_label.set_xalign(0.0);
        let labels: Vec<&str> = CONFLICTS.iter().map(|(_, l)| *l).collect();
        let conflict = gtk::DropDown::from_strings(&labels);
        let scope =
            gtk::CheckButton::with_label("Make the iPod's iprune sync scope what the X3 syncs");
        scope.set_visible(matches!(listing.loaded, Loaded::Ipod(_)));
        let count = gtk::Label::new(None);
        count.set_xalign(0.0);
        let cancel = gtk::Button::with_mnemonic("_Cancel");
        let import = gtk::Button::with_mnemonic("_Import");
        import.add_css_class("suggested-action");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&cancel);
        buttons.append(&import);
        crate::closes(&cancel, dialog);
        for w in [
            filter.upcast_ref::<gtk::Widget>(),
            scroll.upcast_ref(),
            pick.upcast_ref(),
            conflict_label.upcast_ref(),
            conflict.upcast_ref(),
            scope.upcast_ref(),
            count.upcast_ref(),
            buttons.upcast_ref(),
        ] {
            body.append(w);
        }

        let update = {
            let (checked, count, import, total) = (
                checked.clone(),
                count.clone(),
                import.clone(),
                offered.len(),
            );
            Rc::new(move || {
                let n = checked.borrow().len();
                count.set_text(&format!("{n} of {total} checked"));
                import.set_sensitive(n > 0);
            })
        };
        for (check, name) in checks.iter().zip(&offered) {
            let (checked, name, update) = (checked.clone(), name.clone(), update.clone());
            check.connect_toggled(move |c| {
                if c.is_active() {
                    checked.borrow_mut().insert(name.clone());
                } else {
                    checked.borrow_mut().remove(&name);
                }
                update();
            });
        }
        for (button, on) in [(&all, true), (&none, false)] {
            let checks = checks.clone();
            button.connect_clicked(move |_| {
                for c in &checks {
                    c.set_active(on);
                }
            });
        }
        update();

        let loaded = Rc::new(RefCell::new(Some(listing.loaded)));
        {
            let ui = Rc::downgrade(self);
            let (dialog, stack) = (dialog.downgrade(), stack.clone());
            import.connect_clicked(move |_| {
                let (Some(ui), Some(dialog)) = (ui.upgrade(), dialog.upgrade()) else {
                    return;
                };
                let Some(names) = names_to_import(&offered, &checked.borrow()) else {
                    return;
                };
                let Some(loaded) = loaded.borrow_mut().take() else {
                    return;
                };
                let on_conflict = CONFLICTS
                    .get(conflict.selected() as usize)
                    .map_or(OnConflict::Skip, |(c, _)| *c);
                ui.run_import(
                    &dialog,
                    &stack,
                    loaded,
                    names,
                    on_conflict,
                    scope.is_active(),
                );
            });
        }
        stack.add_named(&body, Some("choose"));
        stack.set_visible_child(&body);
    }

    fn run_import(
        self: &Rc<Self>,
        dialog: &gtk::Window,
        stack: &gtk::Stack,
        loaded: Loaded,
        names: Vec<String>,
        on_conflict: OnConflict,
        copy_scope: bool,
    ) {
        working_page(stack, "Importing…");
        // While playlists are written, neither the dialog nor the window
        // closes (see their close handlers), and the application stays up;
        // Sync, Adopt and Eject wait.
        self.writing.set(true);
        self.busy.set(true);
        self.refresh_device();
        dialog.set_deletable(false);
        let hold = self.window.application().map(|a| a.hold());
        let config = self.data.config();
        let paths = self.data.paths().clone();
        let ui = Rc::downgrade(self);
        let (dialog, stack) = (dialog.downgrade(), stack.clone());
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                import_job::run(&config, &paths, loaded, &names, on_conflict, copy_scope)
            })
            .await;
            drop(hold);
            let Some(ui) = ui.upgrade() else { return };
            ui.writing.set(false);
            ui.busy.set(false);
            if let Some(d) = dialog.upgrade() {
                d.set_deletable(true);
            }
            // A scope copied to [sync] changes the config.
            if let Err(e) = ui.data.reload_config() {
                ui.status.set_text(&format!("{e:#}"));
            }
            ui.fill_sidebar();
            ui.refresh_device();
            let Some(dialog) = dialog.upgrade() else {
                return;
            };
            match joined(result) {
                Ok(done) => result_page(
                    &dialog,
                    &stack,
                    match done.failed {
                        0 => "Playlists imported",
                        _ => "Some playlists were not imported",
                    },
                    &done.text,
                ),
                Err(e) => result_page(
                    &dialog,
                    &stack,
                    "The playlists were not imported",
                    &format!("{e:#}"),
                ),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_playlist_checked_imports_all_and_a_part_names_them_once() {
        let offered: Vec<String> = ["Jazz", "Rock", "Jazz"].map(String::from).to_vec();
        let all: BTreeSet<String> = ["Jazz", "Rock"].map(String::from).into();
        assert_eq!(names_to_import(&offered, &all), Some(Vec::new()));
        let some: BTreeSet<String> = ["Jazz".to_owned()].into();
        assert_eq!(
            names_to_import(&offered, &some),
            Some(vec!["Jazz".to_owned()])
        );
        assert_eq!(names_to_import(&offered, &BTreeSet::new()), None);
    }
}
