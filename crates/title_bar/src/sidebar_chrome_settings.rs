use gpui::WindowButtonLayout;
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Copy, Clone, Debug, RegisterSetting)]
pub struct SidebarChromeSettings {
    pub show_onboarding_banner: bool,
    pub show_user_picture: bool,
    pub show_sign_in: bool,
    pub show_user_menu: bool,
    pub show_menus: bool,
    pub button_layout: Option<WindowButtonLayout>,
}

impl Settings for SidebarChromeSettings {
    fn from_settings(s: &SettingsContent) -> Self {
        let content = s.sidebar.clone().unwrap();
        SidebarChromeSettings {
            show_onboarding_banner: content.show_onboarding_banner.unwrap(),
            show_user_picture: content.show_user_picture.unwrap(),
            show_sign_in: content.show_sign_in.unwrap(),
            show_user_menu: content.show_user_menu.unwrap(),
            show_menus: content.show_menus.unwrap(),
            button_layout: content.button_layout.unwrap_or_default().into_layout(),
        }
    }
}
