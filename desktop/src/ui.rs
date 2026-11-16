use crate::platform::{self, WindowCommands, WindowVisibility};
use crate::runtime::{ClientHandle, Snapshot};
use eframe::egui::{self, Context, Frame, Margin, ViewportCommand};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::path::PathBuf;
use std::sync::mpsc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

mod art;
mod panel;
mod theme;

const MASCOT: &[u8] = include_bytes!("../../docs/media/psimole.png");

pub fn run(dir: PathBuf, commands: WindowCommands) -> eframe::Result<()> {
    let image = image::load_from_memory(MASCOT)
        .expect("embedded PsiMole PNG")
        .into_rgba8();
    let (width, height) = image.dimensions();
    let icon = egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([376.0, 654.0])
            .with_min_inner_size([340.0, 460.0])
            .with_icon(icon.clone()),
        vsync: true,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "PsiMole",
        options,
        Box::new(move |creation| Ok(Box::new(Desktop::new(creation, dir, commands, icon)?))),
    )
}

#[derive(Clone, Copy)]
struct Language(bool);

impl Language {
    fn text<'a>(self, english: &'a str, russian: &'a str) -> &'a str {
        if self.0 { russian } else { english }
    }

    fn country(self, code: &str) -> String {
        let names = match code {
            "" => ("Automatic", "Автоматически"),
            "AT" => ("Austria", "Австрия"),
            "AU" => ("Australia", "Австралия"),
            "BE" => ("Belgium", "Бельгия"),
            "CA" => ("Canada", "Канада"),
            "CH" => ("Switzerland", "Швейцария"),
            "CZ" => ("Czechia", "Чехия"),
            "DE" => ("Germany", "Германия"),
            "DK" => ("Denmark", "Дания"),
            "ES" => ("Spain", "Испания"),
            "FI" => ("Finland", "Финляндия"),
            "FR" => ("France", "Франция"),
            "GB" => ("United Kingdom", "Великобритания"),
            "ID" => ("Indonesia", "Индонезия"),
            "IE" => ("Ireland", "Ирландия"),
            "IN" => ("India", "Индия"),
            "IT" => ("Italy", "Италия"),
            "JP" => ("Japan", "Япония"),
            "KR" => ("South Korea", "Южная Корея"),
            "LT" => ("Lithuania", "Литва"),
            "NL" => ("Netherlands", "Нидерланды"),
            "NO" => ("Norway", "Норвегия"),
            "PL" => ("Poland", "Польша"),
            "RO" => ("Romania", "Румыния"),
            "RS" => ("Serbia", "Сербия"),
            "SE" => ("Sweden", "Швеция"),
            "SG" => ("Singapore", "Сингапур"),
            "US" => ("United States", "США"),
            _ => return code.to_owned(),
        };
        self.text(names.0, names.1).to_owned()
    }
}

struct Tray {
    _icon: TrayIcon,
    open: MenuItem,
    quit: MenuItem,
}

impl Tray {
    fn create(
        icon: &egui::IconData,
        hwnd: isize,
        quit: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let image = image::RgbaImage::from_raw(icon.width, icon.height, icon.rgba.clone())
            .expect("embedded icon dimensions");
        let image = image::imageops::resize(&image, 32, 32, image::imageops::FilterType::Lanczos3);
        let menu = Menu::new();
        let open = MenuItem::new("Open PsiMole", true, None);
        let exit = MenuItem::new("Quit", true, None);
        menu.append(&open)?;
        menu.append(&exit)?;
        let tray = TrayIconBuilder::new()
            .with_icon(tray_icon::Icon::from_rgba(image.into_raw(), 32, 32)?)
            .with_tooltip("PsiMole")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        let open_id = open.id().clone();
        let exit_id = exit.id().clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == open_id {
                platform::show_window(hwnd);
            } else if event.id == exit_id {
                quit();
            }
        }));
        let tray_id = tray.id().clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(event, TrayIconEvent::Click { id, button: MouseButton::Left, button_state: MouseButtonState::Up, .. } if id == tray_id)
            {
                platform::show_window(hwnd);
            }
        }));
        Ok(Self {
            _icon: tray,
            open,
            quit: exit,
        })
    }

    fn language(&self, language: Language) {
        self.open
            .set_text(language.text("Open PsiMole", "Открыть PsiMole"));
        self.quit.set_text(language.text("Quit", "Выйти"));
    }
}

struct Desktop {
    client: ClientHandle,
    state: Snapshot,
    art: art::Art,
    hwnd: isize,
    tray: Option<Tray>,
    visibility: Option<WindowVisibility>,
    quit: mpsc::Receiver<()>,
    quitting: bool,
}

impl Desktop {
    fn new(
        creation: &eframe::CreationContext<'_>,
        dir: PathBuf,
        commands: WindowCommands,
        icon: egui::IconData,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let context = &creation.egui_ctx;
        theme::install(context);
        let raw = creation.window_handle()?;
        let RawWindowHandle::Win32(win32) = raw.as_raw() else {
            return Err("Expected a Win32 window".into());
        };
        let hwnd = win32.hwnd.get();
        platform::apply_window_theme(hwnd, theme::BACKGROUND.to_array(), theme::TEXT.to_array());
        let (client, state) = ClientHandle::start(dir, context.clone())?;
        let (request_quit, quit) = mpsc::channel();
        let close = {
            let context = context.clone();
            move || {
                let _ = request_quit.send(());
                // Showing the hidden viewport lets it process its final update.
                platform::show_window(hwnd);
                context.request_repaint();
            }
        };
        let tray = Tray::create(&icon, hwnd, close.clone()).ok();
        let visibility = Some(WindowVisibility::install(hwnd, tray.is_some())?);
        platform::listen_for_window(commands, hwnd, close);
        if let Some(tray) = &tray {
            tray.language(Language(
                state
                    .settings
                    .russian
                    .unwrap_or_else(platform::default_russian),
            ));
        }
        Ok(Self {
            client,
            state,
            art: art::Art::new(context),
            hwnd,
            tray,
            visibility,
            quit,
            quitting: false,
        })
    }

    fn language(&self) -> Language {
        Language(
            self.state
                .settings
                .russian
                .unwrap_or_else(platform::default_russian),
        )
    }
}

impl eframe::App for Desktop {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.quitting {
            return;
        }
        if self.quit.try_recv().is_ok() {
            self.quitting = true;
            self.client.shutdown();
            self.visibility.take();
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        let previous_language = self.language().0;
        self.client.refresh(&mut self.state);
        let language = self.language();
        if previous_language != language.0
            && let Some(tray) = &self.tray
        {
            tray.language(language);
        }
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.panel(ui, language));
            });
    }
}
