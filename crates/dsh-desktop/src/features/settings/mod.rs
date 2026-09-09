//! 设置功能切片:独立页(render + 两栏壳)与 provider CRUD 表单、
//! onboarding,另含侧栏设置模式菜单(menu;自 ui::sidebar 切出)与
//! 全权确认/删除确认模态。

pub(crate) mod store;
mod views;

pub(crate) use store::{FullAccessAsk, SettingsNav, SettingsStore};
pub(crate) use views::{
    full_access_modal, menu, provider_delete_modal, provider_models_fetch_modal, render,
    settings_row,
};
