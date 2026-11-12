//! HGhub 核心模块（无 Tauri 依赖）：业务 API、网络、媒体代理、会话、线路、诊断日志。
//!
//! 桌面端（src-tauri）与网页版（web/ crate）共用。
//! 本 crate 不依赖 Tauri，所以网页版 binary 不需要系统 WebKit/GTK 库就能链接，
//! 容器镜像也可以保持很小。

pub mod api;
pub mod lines;
pub mod logging;
pub mod media;
pub mod net;
pub mod network_policy;
pub mod playback_log;
pub mod session;
