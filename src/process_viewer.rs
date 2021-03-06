//
// Process viewer
//
// Copyright (c) 2017 Guillaume Gomez
//

#![crate_type = "bin"]

extern crate cairo;
extern crate gdk;
extern crate gdk_pixbuf;
extern crate gio;
extern crate glib;
extern crate gtk;
extern crate libc;
extern crate pango;
extern crate sysinfo;

#[macro_use]
extern crate serde;
extern crate serde_any;

use sysinfo::*;

use gdk_pixbuf::Pixbuf;
use gio::{ActionExt, ActionMapExt, ApplicationExt, ApplicationExtManual, MemoryInputStream};
use glib::{Bytes, Cast, IsA, ToVariant};
use gtk::{AboutDialog, Button, Dialog, EditableSignals, Entry, Inhibit, MessageDialog};
use gtk::{
    AboutDialogExt, BoxExt, ButtonExt, ContainerExt, DialogExt, EntryExt, GtkApplicationExt,
    GtkListStoreExt, GtkListStoreExtManual, GtkWindowExt, GtkWindowExtManual, NotebookExtManual,
    SearchBarExt, ToggleButtonExt, TreeModelExt, TreeSortableExtManual, TreeViewExt, WidgetExt,
};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env::args;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::SystemTime;

use display_sysinfo::DisplaySysInfo;
use notebook::NoteBook;
use procs::{create_and_fill_model, Procs};
use settings::Settings;

#[macro_use]
mod macros;

mod color;
mod disk_info;
mod display_sysinfo;
mod graph;
mod notebook;
mod utils;
mod process_dialog;
mod procs;
mod settings;

pub const APPLICATION_NAME: &str = "com.github.GuillaumeGomez.process-viewer";

fn update_system_info(system: &Rc<RefCell<sysinfo::System>>, info: &mut DisplaySysInfo,
                      display_fahrenheit: bool) {
    let mut system = system.borrow_mut();
    system.refresh_system();
    info.update_system_info(&system, display_fahrenheit);
    info.update_system_info_display(&system);
}

fn update_system_network(system: &Rc<RefCell<sysinfo::System>>, info: &mut DisplaySysInfo) {
    let mut system = system.borrow_mut();
    system.refresh_network();
    info.update_network(&system);
}

fn update_window(list: &gtk::ListStore, system: &Rc<RefCell<sysinfo::System>>) {
    let mut system = system.borrow_mut();
    system.refresh_processes();
    let entries: &HashMap<Pid, Process> = system.get_process_list();
    let mut seen: HashSet<Pid> = HashSet::new();

    if let Some(iter) = list.get_iter_first() {
        let mut valid = true;
        while valid {
            if let Some(pid) = list.get_value(&iter, 0).get::<u32>().map(|x| x as Pid) {
                if let Some(p) = entries.get(&(pid)) {
                    list.set(&iter,
                             &[2, 3, 5],
                             &[&format!("{:.1}", p.cpu_usage()), &p.memory(), &p.cpu_usage()]);
                    valid = list.iter_next(&iter);
                    seen.insert(pid);
                } else {
                    valid = list.remove(&iter);
                }
            }
        }
    }

    for (pid, pro) in entries.iter() {
        if !seen.contains(pid) {
            create_and_fill_model(list, pro.pid().as_u32(),
                                  &format!("{:?}", &pro.cmd()), &pro.name(),
                                  pro.cpu_usage(), pro.memory());
        }
    }
}

fn parse_quote(line: &str, quote: char) -> Vec<String> {
    let args = line.split(quote).collect::<Vec<&str>>();
    let mut out_args = vec!();

    for (num, arg) in args.iter().enumerate() {
        if num != 1 {
            out_args.extend_from_slice(&parse_entry(arg));
        } else {
            out_args.push((*arg).to_owned());
        }
    }
    out_args
}

// super simple parsing
fn parse_entry(line: &str) -> Vec<String> {
    match (line.find('\''), line.find('"')) {
        (Some(x), Some(y)) => {
            if x < y {
                parse_quote(line, '\'')
            } else {
                parse_quote(line, '"')
            }
        }
        (Some(_), None) => parse_quote(line, '\''),
        (None, Some(_)) => parse_quote(line, '"'),
        (None, None) => line.split(' ').map(::std::borrow::ToOwned::to_owned)
                                       .collect::<Vec<String>>(),
    }
}

#[cfg(unix)]
fn build_command(c: &mut Command) -> &mut Command {
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    }
}

#[cfg(windows)]
fn build_command(c: &mut Command) -> &mut Command {
    c
}

fn start_detached_process(line: &str) -> Option<String> {
    let args = parse_entry(line);
    let command = args[0].clone();

    let cmd = build_command(Command::new(&command).args(&args)).stdin(Stdio::null())
                                                               .stderr(Stdio::null())
                                                               .stdout(Stdio::null())
                                                               .spawn();
    if cmd.is_err() {
        Some(format!("Failed to start '{}'", &command))
    } else {
        None
    }
}

fn run_command<T: IsA<gtk::Window>>(input: &Entry, window: &T, d: &Dialog) {
    if let Some(text) = input.get_text() {
        let x = if let Some(x) = start_detached_process(&text) {
            x
        } else {
            "The command started successfully".to_owned()
        };
        d.destroy();
        let m = MessageDialog::new(Some(window),
                                   gtk::DialogFlags::DESTROY_WITH_PARENT,
                                   gtk::MessageType::Info,
                                   gtk::ButtonsType::Ok,
                                   &x);
        m.set_modal(true);
        m.connect_response(|dialog, response| {
            if response == gtk::ResponseType::DeleteEvent ||
               response == gtk::ResponseType::Close ||
               response == gtk::ResponseType::Ok {
                dialog.destroy();
            }
        });
        m.show_all();
    }
}

fn create_new_proc_diag(process_dialogs: &Rc<RefCell<HashMap<Pid, process_dialog::ProcDialog>>>,
                        pid: Pid,
                        sys: &sysinfo::System,
                        window: &gtk::ApplicationWindow,
                        running_since: &SystemTime,
                        starting_time: u64,
) {
    let running_since = match running_since.elapsed() {
        Ok(r) => r.as_secs(),
        Err(e) => {
            eprintln!("2. Error when try to get elapsed time: {:?}", e);
            return;
        }
    };
    if process_dialogs.borrow().get(&pid).is_none() {
        let total_memory = sys.get_total_memory();
        if let Some(process) = sys.get_process(pid) {
            let diag = process_dialog::create_process_dialog(process,
                                                             window,
                                                             running_since,
                                                             starting_time,
                                                             total_memory);
            diag.popup.connect_destroy(clone!(process_dialogs => move |_| {
                process_dialogs.borrow_mut().remove(&pid);
            }));
            process_dialogs.borrow_mut().insert(pid, diag);
        }
    }
}

pub struct RequiredForSettings {
    current_source: Option<glib::SourceId>,
    current_network_source: Option<glib::SourceId>,
    current_system_source: Option<glib::SourceId>,
    running_since: Rc<RefCell<SystemTime>>,
    sys: Rc<RefCell<sysinfo::System>>,
    process_dialogs: Rc<RefCell<HashMap<Pid, process_dialog::ProcDialog>>>,
    list_store: gtk::ListStore,
    display_tab: Rc<RefCell<DisplaySysInfo>>,
    start_time: u64,
}

pub fn setup_timeout(
    refresh_time: u32,
    rfs: &Rc<RefCell<RequiredForSettings>>,
) {
    let ret = {
        let mut rfs = rfs.borrow_mut();
        rfs.current_source.take().map(glib::Source::remove);

        let sys = &rfs.sys;
        let running_since = &rfs.running_since;
        let process_dialogs = &rfs.process_dialogs;
        let list_store = &rfs.list_store;
        let start_time = rfs.start_time;

        Some(
            gtk::timeout_add(refresh_time,
                             clone!(running_since, sys, process_dialogs, list_store => move || {
                // first part, deactivate sorting
                let sorted = TreeSortableExtManual::get_sort_column_id(&list_store);
                list_store.set_unsorted();

                // we update the tree view
                update_window(&list_store, &sys);

                // we re-enable the sorting
                if let Some((col, order)) = sorted {
                    list_store.set_sort_column_id(col, order);
                }
                let dialogs = process_dialogs.borrow();
                let running_since = match running_since.borrow().elapsed() {
                    Ok(r) => r.as_secs(),
                    Err(e) => {
                        eprintln!("An error occurred when getting elapsed time: {:?}", e);
                        return glib::Continue(true);
                    }
                };
                for dialog in dialogs.values() {
                    // TODO: handle dead process?
                    if let Some(process) = sys.borrow().get_process(dialog.pid) {
                        dialog.update(process, running_since, start_time);
                    }
                }
                glib::Continue(true)
            }))
        )
    };
    rfs.borrow_mut().current_source = ret;
}

pub fn setup_network_timeout(
    refresh_time: u32,
    rfs: &Rc<RefCell<RequiredForSettings>>,
) {
    let ret = {
        let mut rfs = rfs.borrow_mut();
        rfs.current_network_source.take().map(glib::Source::remove);

        let sys = &rfs.sys;
        let display_tab = &rfs.display_tab;

        Some(
            gtk::timeout_add(refresh_time, clone!(sys, display_tab => move || {
                update_system_network(&sys, &mut display_tab.borrow_mut());
                glib::Continue(true)
            }))
        )
    };
    rfs.borrow_mut().current_network_source = ret;
}

pub fn setup_system_timeout(
    refresh_time: u32,
    rfs: &Rc<RefCell<RequiredForSettings>>,
    settings: &Rc<RefCell<Settings>>,
) {
    let ret = {
        let mut rfs = rfs.borrow_mut();
        rfs.current_system_source.take().map(glib::Source::remove);

        let sys = &rfs.sys;
        let display_tab = &rfs.display_tab;

        Some(
            gtk::timeout_add(refresh_time, clone!(sys, display_tab, settings => move || {
                update_system_info(&sys, &mut display_tab.borrow_mut(), settings.borrow().display_fahrenheit);
                glib::Continue(true)
            }))
        )
    };
    rfs.borrow_mut().current_system_source = ret;
}

fn build_ui(application: &gtk::Application) {
    let settings = Settings::load();

    let menu = gio::Menu::new();
    let menu_bar = gio::Menu::new();
    let more_menu = gio::Menu::new();
    let settings_menu = gio::Menu::new();

    menu.append(Some("Launch new executable"), Some("app.new-task"));
    menu.append(Some("Quit"), Some("app.quit"));

    settings_menu.append(Some("Display temperature in °F"), Some("app.temperature"));
    settings_menu.append(Some("Display graphs"), Some("app.graphs"));
    settings_menu.append(Some("More settings..."), Some("app.settings"));
    menu_bar.append_submenu(Some("_Settings"), &settings_menu);

    more_menu.append(Some("About"), Some("app.about"));
    menu_bar.append_submenu(Some("?"), &more_menu);

    application.set_app_menu(Some(&menu));
    application.set_menubar(Some(&menu_bar));

    let window = gtk::ApplicationWindow::new(application);
    let sys = sysinfo::System::new();
    let start_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
                                      .expect("couldn't get start time")
                                      .as_secs();
    let running_since = Rc::new(RefCell::new(SystemTime::now()));
    let sys = Rc::new(RefCell::new(sys));
    let mut note = NoteBook::new();
    let procs = Procs::new(sys.borrow().get_process_list(), &mut note);
    let current_pid = Rc::clone(&procs.current_pid);
    let info_button = procs.info_button.clone();

    window.set_title("Process viewer");
    window.set_position(gtk::WindowPosition::Center);
    // To silence the annying warning:
    // "(.:2257): Gtk-WARNING **: Allocating size to GtkWindow 0x7f8a31038290 without
    // calling gtk_widget_get_preferred_width/height(). How does the code know the size to
    // allocate?"
    window.get_preferred_width();
    window.set_default_size(500, 700);

    window.connect_delete_event(|w, _| {
        w.destroy();
        Inhibit(false)
    });

    sys.borrow_mut().refresh_all();
    procs.kill_button.connect_clicked(clone!(current_pid, sys => move |_| {
        let sys = sys.borrow();
        if let Some(process) = current_pid.get().and_then(|pid| sys.get_process(pid)) {
            process.kill(Signal::Kill);
        }
    }));

    let display_tab = DisplaySysInfo::new(&sys, &mut note, &window, &settings);
    disk_info::create_disk_info(&sys, &mut note);

    let v_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let ram_check_box = display_tab.ram_check_box.clone();
    let swap_check_box = display_tab.swap_check_box.clone();
    let network_check_box = display_tab.network_check_box.clone();
    let temperature_check_box = display_tab.temperature_check_box.clone();

    let display_tab = Rc::new(RefCell::new(display_tab));

    // I think it's now useless to have this one...
    v_box.pack_start(&note.notebook, true, true, 0);

    window.add(&v_box);

    let process_dialogs: Rc<RefCell<HashMap<Pid, process_dialog::ProcDialog>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let list_store = procs.list_store.clone();

    let rfs = Rc::new(RefCell::new(RequiredForSettings {
        current_source: None,
        current_network_source: None,
        current_system_source: None,
        running_since: running_since.clone(),
        sys: sys.clone(),
        process_dialogs: process_dialogs.clone(),
        list_store,
        display_tab,
        start_time,
    }));

    let refresh_processes_rate = settings.refresh_processes_rate;
    let refresh_system_rate = settings.refresh_system_rate;
    let refresh_network_rate = settings.refresh_network_rate;
    let settings = Rc::new(RefCell::new(settings));
    setup_timeout(refresh_processes_rate, &rfs);
    setup_network_timeout(refresh_network_rate, &rfs);
    setup_system_timeout(refresh_system_rate, &rfs, &settings);

    let settings_action = gio::SimpleAction::new("settings", None);
    settings_action.connect_activate(clone!(application, settings => move |_, _| {
        settings::show_settings_dialog(&application, &settings, &rfs);
    }));

    info_button.connect_clicked(
        clone!(current_pid, window, running_since, process_dialogs, sys => move |_| {
            if let Some(pid) = current_pid.get() {
                create_new_proc_diag(&process_dialogs, pid, &*sys.borrow(), &window,
                                     &*running_since.borrow(),
                                     start_time);
            }
        }
    ));

    procs.left_tree.connect_row_activated(clone!(window, sys => move |tree_view, path, _| {
        let model = tree_view.get_model().expect("couldn't get model");
        let iter = model.get_iter(path).expect("couldn't get iter");
        let pid = model.get_value(&iter, 0)
                       .get::<u32>()
                       .map(|x| x as Pid)
                       .expect("failed to get value from model");
        if process_dialogs.borrow().get(&pid).is_none() {
            create_new_proc_diag(&process_dialogs, pid, &*sys.borrow(), &window,
                                 &*running_since.borrow(),
                                 start_time);
        }
    }));

    let quit = gio::SimpleAction::new("quit", None);
    quit.connect_activate(clone!(window => move |_, _| {
        window.destroy();
    }));

    let about = gio::SimpleAction::new("about", None);
    about.connect_activate(clone!(window => move |_, _| {
        let p = AboutDialog::new();
        p.set_authors(&["Guillaume Gomez"]);
        p.set_website_label(Some("my website"));
        p.set_website(Some("https://guillaume-gomez.fr/"));
        p.set_comments(Some("A process viewer GUI wrote with gtk-rs"));
        p.set_copyright(Some("This is under MIT license"));
        p.set_transient_for(Some(&window));
        p.set_program_name("process-viewer");
        let memory_stream = MemoryInputStream::new_from_bytes(
                                &Bytes::from_static(include_bytes!("../assets/eye.png")));
        let logo = Pixbuf::new_from_stream(&memory_stream, None::<&gio::Cancellable>);
        if let Ok(logo) = logo {
            p.set_logo(Some(&logo));
        }
        p.set_modal(true);
        p.connect_response(|dialog, response| {
            if response == gtk::ResponseType::DeleteEvent ||
               response == gtk::ResponseType::Close {
                dialog.destroy();
            }
        });
        p.show_all();
    }));

    let new_task = gio::SimpleAction::new("new-task", None);
    new_task.connect_activate(clone!(window => move |_, _| {
        let dialog = Dialog::new();
        dialog.set_title("Launch new executable");
        let content_area = dialog.get_content_area();
        let input = Entry::new();
        let run = Button::new_with_label("Run");
        let cancel = Button::new_with_label("Cancel");
        let v_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let h_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        h_box.pack_start(&run, true, true, 0);
        h_box.pack_start(&cancel, true, true, 0);
        v_box.pack_start(&input, true, true, 0);
        v_box.pack_start(&h_box, true, true, 0);
        content_area.add(&v_box);
        dialog.set_transient_for(Some(&window));
        // To silence the annying warning:
        // "(.:2257): Gtk-WARNING **: Allocating size to GtkWindow 0x7f8a31038290 without
        // calling gtk_widget_get_preferred_width/height(). How does the code know the size to
        // allocate?"
        dialog.get_preferred_width();
        dialog.set_size_request(400, 70);
        dialog.show_all();

        run.set_sensitive(false);
        input.connect_changed(clone!(run => move |input| {
            match input.get_text() {
                Some(ref x) if !x.is_empty() => run.set_sensitive(true),
                _ => run.set_sensitive(false),
            }
        }));
        cancel.connect_clicked(clone!(dialog => move |_| {
            dialog.destroy();
        }));
        input.connect_activate(clone!(window, dialog => move |input| {
            run_command(input, &window, &dialog);
        }));
        run.connect_clicked(clone!(window => move |_| {
            run_command(&input, &window, &dialog);
        }));
    }));

    let graphs = gio::SimpleAction::new_stateful("graphs", None,
                                                 &settings.borrow().display_graph.to_variant());
    graphs.connect_activate(clone!(settings => move |g, _| {
        let mut is_active = false;
        if let Some(g) = g.get_state() {
            is_active = g.get().expect("couldn't get bool");
            ram_check_box.set_active(!is_active);
            swap_check_box.set_active(!is_active);
            network_check_box.set_active(!is_active);
            if let Some(ref temperature_check_box) = temperature_check_box {
                temperature_check_box.set_active(!is_active);
            }
        }
        // We need to change the toggle state ourselves. `gio` dark magic.
        g.change_state(&(!is_active).to_variant());

        // We update the setting and save it!
        settings.borrow_mut().display_graph = !is_active;
        settings.borrow().save();
    }));

    let temperature = gio::SimpleAction::new_stateful("temperature", None,
                                                      &settings.borrow()
                                                               .display_fahrenheit
                                                               .to_variant());
    temperature.connect_activate(move |g, _| {
        let mut is_active = false;
        if let Some(g) = g.get_state() {
            is_active = g.get().expect("couldn't get graph state");
        }
        // We need to change the toggle state ourselves. `gio` dark magic.
        g.change_state(&(!is_active).to_variant());

        // We update the setting and save it!
        settings.borrow_mut().display_fahrenheit = !is_active;
        settings.borrow().save();
    });

    application.add_action(&about);
    application.add_action(&graphs);
    application.add_action(&temperature);
    application.add_action(&settings_action);
    application.add_action(&new_task);
    application.add_action(&quit);

    let filter_entry = procs.filter_entry.clone();
    let notebook = note.notebook.clone();

    procs.filter_button.connect_clicked(clone!(filter_entry, window => move |_| {
        if filter_entry.get_visible() {
            filter_entry.hide();
        } else {
            filter_entry.show_all();
            window.set_focus(Some(&filter_entry));
        }
    }));
    window.connect_key_press_event(move |win, key| {
        if notebook.get_current_page() == Some(0) { // the process list
            if key.get_keyval() == gdk::enums::key::Escape {
                procs.hide_filter();
            } else {
                let ret = procs.search_bar.handle_event(key);
                match procs.filter_entry.get_text() {
                    Some(ref s) if s.len() > 0 => {
                        procs.filter_entry.show_all();
                        if win.get_focus() != Some(procs.filter_entry.clone().upcast::<gtk::Widget>()) {
                            win.set_focus(Some(&procs.filter_entry));
                        }
                    }
                    _ => {}
                }
                return Inhibit(ret);
            }
        }
        Inhibit(false)
    });

    application.connect_activate(move |_| {
        window.show_all();
        filter_entry.hide();
        window.present();
    });
}

fn main() {
    let application = gtk::Application::new(Some(APPLICATION_NAME),
                                            gio::ApplicationFlags::empty())
                                       .expect("Initialization failed...");

    application.connect_startup(move |app| {
        build_ui(app);
    });

    glib::set_application_name("process-viewer");
    application.run(&args().collect::<Vec<_>>());
}
