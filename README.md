# Building a Simple S3 Browser with GPUI Kit

GPUI is the GPU-accelerated UI framework extracted from the Zed editor. It gives Rust applications a native desktop shell: windows, input handling, and a retained element tree that renders fast. What it does not give you is a widget set — buttons, lists, and dialogs are yours to build.

[gpui-kit](https://github.com/longbridge/gpui-kit) fills that gap. It is a component library on top of GPUI, shipping the controls a desktop application actually needs: `Button`, `Input`, `Select`, `List`, `DataTable`, `Dialog`, and more, all themeable and keyboard-accessible out of the box.

To verify a UI kit, you need a real but simple application. S3 is a good fit: its data model is just buckets and objects, and the AWS SDK for Rust covers the API surface with a few builder calls.

This article builds a minimal S3 desktop client with gpui-kit: it lists buckets, browses objects by prefix, downloads a file, and uploads a file. The full source lives in `examples/s3_browser/` on the `gpui-s3-paper` branch.

## 1. Start with GPUI Kit

I have been building desktop applications with GPUI recently, so I wanted to start with a very simple `gpui-kit` app.

Not the complex components — just the basics:

```text
gpui-kit
├── List
├── File List
├── Button
└── Dialog
```

<!-- screenshot: the basic gpui-kit components / project layout -->

An application depends on a single crate. GPUI itself comes along as a re-export, so there is no version pair to keep in sync:

```toml
[dependencies]
gpui-kit = { git = "https://github.com/longbridge/gpui-kit", rev = "fb26e61" }
aws-config = "1"
aws-sdk-s3 = "1"
```

Every window wraps its first view in `Root`, which is what makes the dialog and notification layers work:

```rust
gpui_kit::application()
    .with_assets(gpui_kit::assets::Assets)
    .run(move |cx| {
        gpui_kit::init(cx); // first, before anything else
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|cx| S3Browser::new(client, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("open window");
        })
        .detach();
    });
```

## 2. Why S3?

To exercise these components, I needed a real application with a simple data model. S3 fits:

```text
S3
 ↓
List Buckets
 ↓
Click Bucket
 ↓
List Files
 ↓
Get / Upload File
```

<!-- screenshot: initial S3 Browser window, buckets on the left, file list on the right -->

The client comes from the standard AWS provider chain — environment variables, `~/.aws/credentials`, or instance metadata — so there is no connection form to build:

```rust
let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
let client = Client::new(&config);

let output = client.list_buckets().send().await?;
let buckets: Vec<String> = output
    .buckets()
    .iter()
    .map(|bucket| bucket.name().to_string())
    .collect();
```

## 3. Build a Simple S3 Browser

The first version only does:

```text
List Buckets
List Files
Get File
Upload File
```

<!-- screenshot: object list after clicking a bucket -->

Listing objects turns the flat key space into folders with a `delimiter`, which is exactly what the file list renders:

```rust
let output = client
    .list_objects_v2()
    .bucket(&bucket)
    .prefix(&prefix)
    .delimiter("/")
    .send()
    .await?;

// output.common_prefixes() -> folder rows
// output.contents()        -> file rows
```

Clicking a file opens a gpui-kit `Dialog` to confirm the download:

```rust
window.open_dialog(cx, move |dialog, _, _| {
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
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            // get_object, write to disk
                        }),
                ),
        )
});
```

and the download itself is one `get_object` call:

```rust
let output = client.get_object().bucket(&bucket).key(&key).send().await?;
let data = output.body.collect().await?.into_bytes();
std::fs::write(&file_name, &data)?;
```

Upload uses GPUI's native file picker, then `put_object`:

```rust
let rx = cx.prompt_for_paths(PathPromptOptions {
    files: true,
    directories: false,
    multiple: false,
    prompt: Some("Choose a file to upload".into()),
});

let body = ByteStream::from_path(&path).await?;
client.put_object().bucket(&bucket).key(&key).body(body).send().await?;
```

One detail is worth noting: the AWS SDK is tokio-based while GPUI runs on its own executor. The bridge is a shared tokio runtime wrapped in `smol::unblock`, so the UI thread never blocks on the network:

```rust
cx.spawn(async move |this, cx| {
    let result = smol::unblock(move || {
        runtime().block_on(async move { client.list_buckets().send().await })
    })
    .await;
    this.update(cx, |this, cx| {
        this.buckets = /* ... */;
        cx.notify();
    })
    .ok();
})
.detach();
```

## 4. Build More with the Same Kit

Once the S3 Browser works, the UI pieces stand on their own and can be reused directly:

```text
gpui-kit
    ↓
S3 Browser
    ↓
Next Desktop App
```

<!-- screenshot: the S3 Browser window with gpui-kit components highlighted -->

The whole window is a composition of the same few components:

```rust
div().flex().flex_col().size_full()
    .child(header)
    .child(
        h_flex()
            .child(self.render_bucket_list(cx))  // List
            .child(self.render_entry_list(cx)),  // File List
    )
    .child(self.render_status_bar(cx))
    .children(Root::render_dialog_layer(window, cx)) // Dialog
```

The point is not to build a complete UI framework.

It is:

> Build the kit first, then use it to build simple desktop applications.

## Run it

```bash
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_REGION=us-east-1

cd examples/s3_browser
cargo run
```

Downloaded files land in the current working directory; uploaded files keep their local file name under the prefix you are browsing.
