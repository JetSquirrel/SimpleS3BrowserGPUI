//! A minimal S3 browser built with GPUI Kit.
//!
//! Lists buckets on the left, objects on the right, and supports download
//! (click a file, confirm in a dialog) and upload (toolbar button, native
//! file picker). Credentials come from the standard AWS provider chain
//! (env vars, ~/.aws, instance metadata).

use std::path::PathBuf;
use std::sync::OnceLock;

use aws_sdk_s3::Client;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Root, Sizable, StyledExt, WindowExt,
    button::{Button, ButtonVariants},
    dialog::DialogFooter,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

/// The AWS SDK is tokio-based while GPUI runs on its own executor, so AWS
/// calls run inside a shared tokio runtime wrapped in `smol::unblock`.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

/// One row in the object list: a "folder" (common prefix) or a file.
#[derive(Clone)]
struct Entry {
    key: String,
    size: i64,
    is_prefix: bool,
}

impl Entry {
    /// Display name relative to the current prefix.
    fn name(&self, prefix: &str) -> &str {
        self.key
            .strip_prefix(prefix)
            .unwrap_or(&self.key)
            .trim_end_matches('/')
    }
}

struct S3Browser {
    client: Client,
    buckets: Vec<String>,
    bucket: Option<String>,
    prefix: String,
    entries: Vec<Entry>,
    loading: bool,
    status: String,
}

impl S3Browser {
    fn new(client: Client, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            client,
            buckets: Vec::new(),
            bucket: None,
            prefix: String::new(),
            entries: Vec::new(),
            loading: false,
            status: String::new(),
        };
        this.load_buckets(cx);
        this
    }

    fn load_buckets(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        self.loading = true;
        self.status = "Listing buckets…".into();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                runtime().block_on(async move { client.list_buckets().send().await })
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(output) => {
                        this.buckets = output
                            .buckets()
                            .iter()
                            .map(|bucket| bucket.name().unwrap_or_default().to_string())
                            .collect();
                        this.buckets.sort();
                        this.status = format!("{} buckets", this.buckets.len());
                    }
                    Err(err) => this.status = format!("{err}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_objects(&mut self, bucket: String, prefix: String, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let shown_bucket = bucket.clone();
        let shown_prefix = prefix.clone();
        self.loading = true;
        self.status = format!("Listing s3://{bucket}/{prefix}");
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                runtime().block_on(async move {
                    client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .prefix(&prefix)
                        .delimiter("/")
                        .send()
                        .await
                })
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(output) => {
                        let mut entries: Vec<Entry> = output
                            .common_prefixes()
                            .iter()
                            .map(|prefix| Entry {
                                key: prefix.prefix().unwrap_or_default().to_string(),
                                size: 0,
                                is_prefix: true,
                            })
                            .collect();
                        entries.extend(
                            output
                                .contents()
                                .iter()
                                // A "folder marker" object duplicates its own prefix entry.
                                .filter(|object| object.key() != Some(shown_prefix.as_str()))
                                .map(|object| Entry {
                                    key: object.key().unwrap_or_default().to_string(),
                                    size: object.size().unwrap_or(0),
                                    is_prefix: false,
                                }),
                        );
                        entries.sort_by(|a, b| {
                            b.is_prefix.cmp(&a.is_prefix).then(a.key.cmp(&b.key))
                        });
                        this.status = format!("{} entries", entries.len());
                        this.entries = entries;
                        this.bucket = Some(shown_bucket);
                        this.prefix = shown_prefix;
                    }
                    Err(err) => this.status = format!("{err}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_bucket(&mut self, bucket: String, cx: &mut Context<Self>) {
        self.load_objects(bucket, String::new(), cx);
    }

    /// Confirm the download in a dialog, then save next to the working
    /// directory under the object's file name.
    fn confirm_download(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let key = entry.key.clone();
        let name = entry.name(&self.prefix).to_string();
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let key = key.clone();
            dialog
                .title(format!("Download {name}"))
                .child(format!("Save s3://{bucket}/{key} to the current directory?"))
                .footer(
                    DialogFooter::new()
                        .gap_2()
                        .child(
                            Button::new("cancel-download")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("confirm-download")
                                .primary()
                                .label("Download")
                                .on_click({
                                    let key = key.clone();
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                        view.update(cx, |this, cx| {
                                            this.download(key.clone(), cx);
                                        });
                                    }
                                }),
                        ),
                )
        });
    }

    fn download(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let client = self.client.clone();
        let file_name = key.rsplit('/').next().unwrap_or(&key).to_string();
        self.loading = true;
        self.status = format!("Downloading {file_name}…");
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                runtime().block_on(async move {
                    let output = client
                        .get_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await?;
                    let data = output.body.collect().await?.into_bytes();
                    std::fs::write(&file_name, &data)?;
                    anyhow::Ok(file_name)
                })
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                this.status = match result {
                    Ok(file_name) => format!("Downloaded {file_name}"),
                    Err(err) => format!("{err:#}"),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pick a local file with the native dialog and upload it under the
    /// current prefix, then refresh the listing.
    fn pick_and_upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a file to upload".into()),
        });
        let view = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = rx.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                cx.update(move |_, cx| {
                    view.update(cx, |this, cx| this.upload(path, cx));
                })
                .ok();
            })
            .detach();
    }

    fn upload(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let Some(file_name) = path.file_name().map(|name| name.to_string_lossy().to_string())
        else {
            return;
        };
        let key = format!("{}{file_name}", self.prefix);
        let client = self.client.clone();
        let prefix = self.prefix.clone();
        let shown_key = key.clone();
        self.loading = true;
        self.status = format!("Uploading {file_name}…");
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                runtime().block_on(async move {
                    let body = aws_sdk_s3::primitives::ByteStream::from_path(&path).await?;
                    client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(body)
                        .send()
                        .await?;
                    anyhow::Ok(())
                })
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(()) => {
                        this.status = format!("Uploaded {shown_key}");
                        if let Some(bucket) = this.bucket.clone() {
                            this.load_objects(bucket, prefix, cx);
                        }
                    }
                    Err(err) => this.status = format!("{err:#}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_bucket_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted;
        let muted_foreground = cx.theme().muted_foreground;
        let mut list = v_flex()
            .id("bucket-list")
            .flex_1()
            .overflow_y_scroll()
            .p_2()
            .gap_1();
        for name in &self.buckets {
            let selected = self.bucket.as_deref() == Some(name.as_str());
            let name = name.clone();
            let label = name.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("bucket-{name}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(muted))
                    .hover(move |style| style.bg(muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_bucket(name.clone(), cx);
                    }))
                    .child(
                        Icon::new(IconName::Inbox)
                            .small()
                            .text_color(muted_foreground),
                    )
                    .child(label.clone()),
            );
        }
        v_flex()
            .w(px(220.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().text_color(muted_foreground).child("Buckets"))
                    .child(
                        Button::new("refresh-buckets")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RotateCw)
                            .on_click(cx.listener(|this, _, _, cx| this.load_buckets(cx))),
                    ),
            )
            .child(list)
    }

    fn render_breadcrumb(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        let mut bar = h_flex()
            .flex_1()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .child(
                div()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child("s3://"),
            );
        if let Some(bucket) = self.bucket.clone() {
            bar = bar.child(
                Button::new("crumb-bucket")
                    .ghost()
                    .xsmall()
                    .label(bucket.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_bucket(bucket.clone(), cx);
                    })),
            );
            // Prefix segments accumulate: a/b/c/ -> [a/, a/b/, a/b/c/].
            let mut accumulated = String::new();
            for segment in self.prefix.split('/').filter(|segment| !segment.is_empty()) {
                accumulated.push_str(segment);
                accumulated.push('/');
                let target = accumulated.clone();
                bar = bar
                    .child(div().text_sm().text_color(muted_foreground).child("/"))
                    .child(
                        Button::new(SharedString::from(format!("crumb-{target}")))
                            .ghost()
                            .xsmall()
                            .label(segment.to_string())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some(bucket) = this.bucket.clone() else {
                                    return;
                                };
                                this.load_objects(bucket, target.clone(), cx);
                            })),
                    );
            }
        }
        bar
    }

    fn render_entry_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted;
        let muted_foreground = cx.theme().muted_foreground;
        let mut list = v_flex()
            .id("entry-list")
            .flex_1()
            .overflow_y_scroll()
            .p_2()
            .gap_1();
        if self.bucket.is_none() {
            list = list.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child("Pick a bucket on the left to browse its objects."),
            );
        } else if self.entries.is_empty() && !self.loading {
            list = list.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child("This prefix is empty."),
            );
        }
        let prefix = self.prefix.clone();
        for entry in self.entries.clone() {
            let name = entry.name(&prefix).to_string();
            let row = div()
                .id(SharedString::from(format!("entry-{}", entry.key)))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .cursor_pointer()
                .hover(move |style| style.bg(muted))
                .child(
                    Icon::new(if entry.is_prefix {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .small()
                    .text_color(muted_foreground),
                )
                .child(div().flex_1().overflow_hidden().child(name))
                .when(!entry.is_prefix, |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(muted_foreground)
                            .child(format_size(entry.size)),
                    )
                });
            let row = if entry.is_prefix {
                let key = entry.key.clone();
                row.on_click(cx.listener(move |this, _, _, cx| {
                    let Some(bucket) = this.bucket.clone() else {
                        return;
                    };
                    this.load_objects(bucket, key.clone(), cx);
                }))
            } else {
                let entry = entry.clone();
                row.on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_download(entry.clone(), window, cx);
                }))
            };
            list = list.child(row);
        }
        list
    }

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .when(self.loading, |this| {
                this.child(Icon::new(IconName::LoaderCircle).small())
            })
            .child(self.status.clone())
    }
}

impl Render for S3Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.bucket.is_some();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(Icon::new(IconName::Globe))
                    .child(div().font_semibold().child("S3 Browser"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("gpui-kit demo"),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_bucket_list(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .px_3()
                                    .py_2()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(self.render_breadcrumb(cx))
                                    .child(
                                        Button::new("refresh-objects")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::RotateCw)
                                            .when(!connected, |this| this.disabled(true))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let Some(bucket) = this.bucket.clone() else {
                                                    return;
                                                };
                                                let prefix = this.prefix.clone();
                                                this.load_objects(bucket, prefix, cx);
                                            })),
                                    )
                                    .child(
                                        Button::new("upload")
                                            .xsmall()
                                            .icon(IconName::Plus)
                                            .label("Upload")
                                            .when(!connected, |this| this.disabled(true))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.pick_and_upload(window, cx);
                                            })),
                                    ),
                            )
                            .child(self.render_entry_list(cx)),
                    ),
            )
            .child(self.render_status_bar(cx))
            .children(Root::render_dialog_layer(window, cx))
    }
}

fn format_size(size: i64) -> String {
    if size >= 1 << 30 {
        format!("{:.1} GB", size as f64 / (1 << 30) as f64)
    } else if size >= 1 << 20 {
        format!("{:.1} MB", size as f64 / (1 << 20) as f64)
    } else if size >= 1 << 10 {
        format!("{:.1} KB", size as f64 / (1 << 10) as f64)
    } else {
        format!("{size} B")
    }
}

fn main() {
    let client = runtime().block_on(async {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Client::new(&config)
    });

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
            cx.spawn(async move |cx| {
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| S3Browser::new(client, cx));
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("open window");
            })
            .detach();
        });
}
