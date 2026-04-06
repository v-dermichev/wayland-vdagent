//! Unified enum wrapper over `wlr_data_control_v1` (legacy) and `ext_data_control_v1` (stable).
//! Both protocols share an identical API, so each method is just a two-arm `match`.

use std::os::fd::BorrowedFd;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Dispatch, QueueHandle};

pub use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
pub use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

pub enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

pub enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

pub enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

pub enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl Manager {
    pub fn create_data_source<
        D: Dispatch<ExtDataControlSourceV1, ()> + Dispatch<ZwlrDataControlSourceV1, ()> + 'static,
    >(
        &self,
        qh: &QueueHandle<D>,
    ) -> Source {
        match self {
            Manager::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
            Manager::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
        }
    }

    pub fn get_data_device<
        D: Dispatch<ExtDataControlDeviceV1, ()> + Dispatch<ZwlrDataControlDeviceV1, ()> + 'static,
    >(
        &self,
        seat: &WlSeat,
        qh: &QueueHandle<D>,
    ) -> Device {
        match self {
            Manager::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Manager::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        }
    }
}

impl Device {
    pub fn set_selection(&self, source: Option<&Source>) {
        match (self, source) {
            (Device::Ext(d), Some(Source::Ext(s))) => d.set_selection(Some(s)),
            (Device::Ext(d), None) => d.set_selection(None),
            (Device::Wlr(d), Some(Source::Wlr(s))) => d.set_selection(Some(s)),
            (Device::Wlr(d), None) => d.set_selection(None),
            // Mismatched variants can't occur — source and device come from the same manager.
            _ => {}
        }
    }
}

impl Source {
    pub fn offer(&self, mime_type: String) {
        match self {
            Source::Ext(s) => s.offer(mime_type),
            Source::Wlr(s) => s.offer(mime_type),
        }
    }

    pub fn destroy(self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
}

impl Offer {
    pub fn receive(&self, mime_type: String, fd: BorrowedFd<'_>) {
        match self {
            Offer::Ext(o) => o.receive(mime_type, fd),
            Offer::Wlr(o) => o.receive(mime_type, fd),
        }
    }

    pub fn destroy(self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

pub const EXT_MANAGER_INTERFACE: &str = "ext_data_control_manager_v1";
pub const WLR_MANAGER_INTERFACE: &str = "zwlr_data_control_manager_v1";
