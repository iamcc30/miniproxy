// MiniProxy 原生外壳：一个 NSWindow + WKWebView，加载本机后端的界面。
//
// 打包后 Bundle 结构：
//   Contents/MacOS/MiniProxy     ← 本外壳（CFBundleExecutable）
//   Contents/MacOS/miniproxy-bin ← 真正的 Rust 后端（外壳负责拉起它）
//
// 行为：
//   - 启动时若 9000 已有 MiniProxy（比如上次被强杀后剩下的无头实例），直接「收养」，
//     不再起子进程；否则 spawn miniproxy-bin，stdout/stderr 追加到 ~/.miniproxy/miniproxy.log
//   - 轮询 /api/health，就绪后在窗口里加载界面；60 次（约 30s）仍不就绪则弹窗报错退出
//   - Cmd+Q / 关闭窗口 / Dock 退出 → 先 POST /api/quit 让后端优雅停机（恢复系统代理），
//     等子进程退出后再退出外壳；若后端是收养的，则不动它
//   - WKWebView 不实现 WKUIDelegate 时会**静默吞掉** window.alert / confirm / prompt
//     （confirm 恒返回 false），界面上「系统代理」「退出」这类带确认的按钮就变成
//     "点了没反应"。所以下面用 NSAlert 把三个对话框都接回来。
//   - WKWebView 也**不会自己下载**：后端 /api/ca.crt（application/x-x509-ca-cert）和
//     /api/export（Content-Disposition: attachment）这类响应没人接管，表现同样是
//     "点了没反应"。下面在 WKNavigationDelegate 里识别下载，改用**外壳自己的
//     NSURLSession** 抓（不用 WebKit 下载器：它在受限环境下拿不到写文件的 sandbox
//     扩展，且只吐到临时文件），下完再用保存面板让用户定落点；
//     blob: 这类没法用 NSURLSession 的仍交回 WKDownload 兜底（落 ~/Downloads）。
//
// 编译（见 packaging/package-macos.sh）：
//   clang -fobjc-arc -O2 -framework Cocoa -framework WebKit AppShell.m -o MiniProxy

#import <Cocoa/Cocoa.h>
#import <WebKit/WebKit.h>
#import <objc/runtime.h>

static NSString *const kBaseURL = @"http://127.0.0.1:9000";

// 把 WKDownload 和它最终落盘的路径绑在一起，供 downloadDidFinish: 使用
static const void *kDestKey = &kDestKey;

#pragma mark - 同步 HTTP（启动/退出阶段用，量小无所谓）

static NSData *syncRequest(NSString *path, NSString *method, NSTimeInterval timeout) {
    NSURL *url = [NSURL URLWithString:[kBaseURL stringByAppendingString:path]];
    NSMutableURLRequest *req = [NSMutableURLRequest requestWithURL:url];
    req.timeoutInterval = timeout;
    req.HTTPMethod = method;
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    return [NSURLConnection sendSynchronousRequest:req returningResponse:nil error:nil];
#pragma clang diagnostic pop
}

static BOOL healthOK(void) {
    NSData *d = syncRequest(@"/api/health", @"GET", 1.0);
    return d.length > 0 && memmem(d.bytes, d.length, "miniproxy", strlen("miniproxy")) != NULL;
}

#pragma mark - AppDelegate

@interface AppDelegate : NSObject <NSApplicationDelegate, NSWindowDelegate, WKUIDelegate,
                                   WKNavigationDelegate, WKDownloadDelegate>
@property(strong) NSWindow *window;
@property(strong) WKWebView *wk;
@property(strong) NSTask *child;   // nil = 收养的已有实例
@property BOOL adopted;
@property BOOL terminating;        // 防止退出流程重入（看门狗 timer 与 terminate 并发）
@property NSInteger tries;
@end

@implementation AppDelegate

- (void)applicationDidFinishLaunching:(NSNotification *)note {
    [self startBackend];
    [self buildUI];
    self.tries = 0;
    [NSTimer scheduledTimerWithTimeInterval:0.5
                                     target:self
                                   selector:@selector(pollHealth:)
                                   userInfo:nil
                                    repeats:YES];
    // 后端没了（界面里点了「⏻ 退出」，或后端崩溃）→ 外壳别留僵尸窗口，跟着退出
    [NSTimer scheduledTimerWithTimeInterval:2.0
                                     target:self
                                   selector:@selector(watchChild:)
                                   userInfo:nil
                                    repeats:YES];
}

- (void)watchChild:(NSTimer *)timer {
    if (self.terminating || self.adopted || !self.child) return;
    if (!self.child.isRunning) {
        self.terminating = YES;
        [NSApp terminate:nil];
    }
}

// 后端就绪才加载界面，避免 WKWebView 吃到 404/空响应
- (void)pollHealth:(NSTimer *)timer {
    if (healthOK()) {
        [timer invalidate];
        [self.wk loadRequest:[NSURLRequest requestWithURL:[NSURL URLWithString:kBaseURL]]];
        return;
    }
    self.tries += 1;
    if (self.tries > 60) {
        [timer invalidate];
        NSAlert *a = [NSAlert new];
        a.messageText = @"MiniProxy 启动失败";
        a.informativeText = @"后端 30 秒内没有就绪。详情见日志：\n~/.miniproxy/miniproxy.log\n"
                             "常见原因：9000/34567 端口被其他程序占用。";
        [a addButtonWithTitle:@"好"];
        [a runModal];
        [NSApp terminate:nil];
    }
}

- (void)startBackend {
    // 已有实例在跑（上次强杀剩下的无头后端，或终端里 cargo run 的开发实例）：收养
    if (healthOK()) {
        self.adopted = YES;
        return;
    }

    NSString *exe = [[NSBundle mainBundle] executablePath];   // .../Contents/MacOS/MiniProxy
    NSString *dir = [exe stringByDeletingLastPathComponent];
    NSString *bin = [dir stringByAppendingPathComponent:@"miniproxy-bin"];

    NSTask *t = [NSTask new];
    t.executableURL = [NSURL fileURLWithPath:bin];

    // 子进程输出追加到日志文件（与打包前的启动器行为一致）
    NSString *logPath = [NSHomeDirectory() stringByAppendingPathComponent:@".miniproxy/miniproxy.log"];
    [[NSFileManager defaultManager] createDirectoryAtPath:[logPath stringByDeletingLastPathComponent]
                              withIntermediateDirectories:YES
                                               attributes:nil
                                                    error:nil];
    [[NSFileManager defaultManager] createFileAtPath:logPath contents:nil attributes:nil];
    NSFileHandle *fh = [NSFileHandle fileHandleForWritingAtPath:logPath];
    [fh seekToEndOfFile];
    t.standardOutput = fh;
    t.standardError = fh;

    NSError *err = nil;
    if (![t launchAndReturnError:&err]) {
        NSAlert *a = [NSAlert new];
        a.messageText = @"无法启动 MiniProxy 后端";
        a.informativeText = [NSString stringWithFormat:@"%@\n\n路径：%@", err.localizedDescription, bin];
        [a addButtonWithTitle:@"好"];
        [a runModal];
        [NSApp terminate:nil];
        return;
    }
    self.child = t;
}

- (void)buildUI {
    NSRect frame = NSMakeRect(0, 0, 1280, 860);
    self.window = [[NSWindow alloc] initWithContentRect:frame
                                              styleMask:NSWindowStyleMaskTitled
                                                      | NSWindowStyleMaskClosable
                                                      | NSWindowStyleMaskMiniaturizable
                                                      | NSWindowStyleMaskResizable
                                                backing:NSBackingStoreBuffered
                                                  defer:NO];
    self.window.title = @"MiniProxy 抓包代理";
    self.window.minSize = NSMakeSize(900, 600);
    [self.window center];
    self.window.delegate = self;

    WKWebViewConfiguration *cfg = [WKWebViewConfiguration new];
    self.wk = [[WKWebView alloc] initWithFrame:self.window.contentView.bounds configuration:cfg];
    self.wk.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    self.wk.allowsBackForwardNavigationGestures = NO;
    self.wk.UIDelegate = self;   // 没有它，界面里的 window.confirm/alert 会被静默丢弃
    self.wk.navigationDelegate = self;   // 下载（CA 证书 / 导出文件）全靠它接管
    self.window.contentView = self.wk;

    [self buildMenu];
    [self.window makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
}

- (void)buildMenu {
    NSMenu *menubar = [NSMenu new];
    NSMenuItem *appItem = [NSMenuItem new];
    [menubar addItem:appItem];
    NSMenu *appMenu = [NSMenu new];

    [appMenu addItemWithTitle:@"关于 MiniProxy"
                       action:@selector(orderFrontStandardAboutPanel:)
                keyEquivalent:@""];

    // 在浏览器里再开一份界面（有时大屏调试更方便）
    NSMenuItem *browser = [appMenu addItemWithTitle:@"在浏览器打开界面"
                                             action:@selector(openInBrowser:)
                                      keyEquivalent:@"b"];
    browser.keyEquivalentModifierMask = NSEventModifierFlagCommand;

    [appMenu addItem:[NSMenuItem separatorItem]];
    [appMenu addItemWithTitle:@"退出 MiniProxy"
                       action:@selector(terminate:)
                keyEquivalent:@"q"];

    appItem.submenu = appMenu;
    NSApp.mainMenu = menubar;
}

- (void)openInBrowser:(id)sender {
    [[NSWorkspace sharedWorkspace] openURL:[NSURL URLWithString:kBaseURL]];
}

#pragma mark - WKUIDelegate（把 JS alert / confirm / prompt 接回来）

// WKWebView 与旧 UIWebView 不同：不实现这些回调时不会弹任何框，
// confirm 直接返回 false、prompt 返回 null，调用方看起来就是"点了没反应"。
- (NSAlert *)alertWithMessage:(NSString *)message {
    NSAlert *a = [NSAlert new];
    a.messageText = @"MiniProxy";
    a.informativeText = message.length > 0 ? message : @"";
    return a;
}

// 窗口在就挂成 sheet（不阻塞主线程），否则退回模态
- (void)presentAlert:(NSAlert *)alert completion:(void (^)(NSModalResponse))completion {
    if (self.window && self.window.isVisible) {
        [alert beginSheetModalForWindow:self.window completionHandler:completion];
    } else {
        completion([alert runModal]);
    }
}

- (void)webView:(WKWebView *)webView
    runJavaScriptAlertPanelWithMessage:(NSString *)message
                      initiatedByFrame:(WKFrameInfo *)frame
                     completionHandler:(void (^)(void))completionHandler {
    NSAlert *a = [self alertWithMessage:message];
    [a addButtonWithTitle:@"好"];
    [self presentAlert:a completion:^(NSModalResponse r) {
        (void)r;
        completionHandler();
    }];
}

- (void)webView:(WKWebView *)webView
    runJavaScriptConfirmPanelWithMessage:(NSString *)message
                        initiatedByFrame:(WKFrameInfo *)frame
                       completionHandler:(void (^)(BOOL))completionHandler {
    NSAlert *a = [self alertWithMessage:message];
    [a addButtonWithTitle:@"确定"];   // 第一个按钮 = 确定
    [a addButtonWithTitle:@"取消"];   // ESC / 关闭窗口走这里 → NO
    [self presentAlert:a completion:^(NSModalResponse r) {
        completionHandler(r == NSAlertFirstButtonReturn);
    }];
}

- (void)webView:(WKWebView *)webView
    runJavaScriptTextInputPanelWithPrompt:(NSString *)prompt
                              defaultText:(NSString *)defaultText
                         initiatedByFrame:(WKFrameInfo *)frame
                        completionHandler:(void (^)(NSString *_Nullable))completionHandler {
    NSAlert *a = [self alertWithMessage:prompt];
    NSTextField *tf = [[NSTextField alloc] initWithFrame:NSMakeRect(0, 0, 320, 24)];
    tf.stringValue = defaultText.length > 0 ? defaultText : @"";
    a.accessoryView = tf;
    [a addButtonWithTitle:@"确定"];
    [a addButtonWithTitle:@"取消"];
    [self presentAlert:a completion:^(NSModalResponse r) {
        completionHandler(r == NSAlertFirstButtonReturn ? tf.stringValue : nil);
    }];
}

// target="_blank" / window.open：WKWebView 默认不建新窗口，不实现这个方法就是"点了没反应"
- (nullable WKWebView *)webView:(WKWebView *)webView
    createWebViewWithConfiguration:(WKWebViewConfiguration *)configuration
               forNavigationAction:(WKNavigationAction *)navigationAction
                    windowFeatures:(WKWindowFeatures *)windowFeatures {
    if (navigationAction.targetFrame != nil) return nil;
    NSURL *url = navigationAction.request.URL;
    if (url == nil) return nil;
    BOOL local = [url.host isEqualToString:@"127.0.0.1"] || [url.host isEqualToString:@"localhost"];
    if (local) {
        // 同源（界面/API）：就在本窗口导航；若响应是附件，下面的
        // decidePolicyForNavigationResponse: 会把它转成下载
        [webView loadRequest:navigationAction.request];
    } else {
        [[NSWorkspace sharedWorkspace] openURL:url];   // 外部链接交给默认浏览器
    }
    return nil;   // 不创建第二个窗口
}

#pragma mark - WKNavigationDelegate：把 WebKit 显示不了的东西变成下载

// ~/Downloads 下挑一个不撞名的路径（同名追加 -1、-2…）
static NSURL *uniqueDestination(NSString *dir, NSString *filename) {
    NSString *base = filename.length > 0 ? filename : @"miniproxy-download";
    NSString *stem = [base stringByDeletingPathExtension];
    NSString *ext = [base pathExtension];
    NSFileManager *fm = [NSFileManager defaultManager];
    for (int i = 0; i < 1000; i++) {
        NSString *name = base;
        if (i > 0) {
            name = ext.length > 0 ? [NSString stringWithFormat:@"%@-%d.%@", stem, i, ext]
                                  : [NSString stringWithFormat:@"%@-%d", stem, i];
        }
        NSString *p = [dir stringByAppendingPathComponent:name];
        if (![fm fileExistsAtPath:p]) return [NSURL fileURLWithPath:p];
    }
    return [NSURL fileURLWithPath:[dir stringByAppendingPathComponent:base]];
}

// ---------- 下载本体 ----------
// 不走 WebKit 自带的下载器：WKDownload 在受限环境下拿不到写文件的 sandbox 扩展
// （sandbox_extension_issue_file failed），而且它只吐到临时文件、进度和文件名都难控。
// 这里所有下载都打到本机后端（127.0.0.1），外壳自己用 NSURLSession 抓更直接可靠。

- (NSString *)downloadDirectory {
    NSString *dir =
        [NSSearchPathForDirectoriesInDomains(NSDownloadsDirectory, NSUserDomainMask, YES)
            firstObject];
    return dir.length > 0 ? dir : NSTemporaryDirectory();
}

// 优先用 Content-Disposition 里的 filename，其次取 URL 末段
static NSString *filenameFromResponse(NSURLResponse *response) {
    NSHTTPURLResponse *http = (NSHTTPURLResponse *)response;
    if ([http isKindOfClass:[NSHTTPURLResponse class]]) {
        id cd = http.allHeaderFields[@"Content-Disposition"];
        if ([cd isKindOfClass:[NSString class]]) {
            NSRange r = [cd rangeOfString:@"filename=" options:NSCaseInsensitiveSearch];
            if (r.location != NSNotFound) {
                NSString *v = [[cd substringFromIndex:NSMaxRange(r)]
                    stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceCharacterSet]];
                if ([v hasPrefix:@"\""]) {
                    NSRange end = [v rangeOfString:@"\"" options:0 range:NSMakeRange(1, v.length - 1)];
                    if (end.location != NSNotFound) {
                        v = [v substringWithRange:NSMakeRange(1, end.location - 1)];
                    }
                }
                if (v.length > 0) return v;
            }
        }
    }
    if (response.URL.lastPathComponent.length > 0) return response.URL.lastPathComponent;
    return @"miniproxy-download";
}

- (void)reportDownloadDone:(NSURL *)dst {
    NSAlert *a = [NSAlert new];
    a.messageText = @"下载完成";
    a.informativeText = [NSString stringWithFormat:@"已保存到：\n%@", dst.path];
    [a addButtonWithTitle:@"在访达中显示"];
    [a addButtonWithTitle:@"好"];
    [self presentAlert:a completion:^(NSModalResponse r) {
        if (r == NSAlertFirstButtonReturn) {
            [[NSWorkspace sharedWorkspace] activateFileViewerSelectingURLs:@[ dst ]];
        }
    }];
}

- (void)reportDownloadFailure:(NSError *)error {
    NSAlert *a = [NSAlert new];
    a.messageText = @"下载失败";
    a.informativeText = error.localizedDescription ?: @"未知错误";
    [a addButtonWithTitle:@"好"];
    [self presentAlert:a completion:^(NSModalResponse r) {
        (void)r;
    }];
}

// 下载完先落到自己的临时文件：NSURLSession 的 tmp 在 completionHandler 返回后就被删，
// 而"存到哪"要等用户在保存面板里点完才知道
static NSString *stagingPathFor(NSString *filename) {
    NSString *safe = filename.length > 0 ? filename : @"miniproxy-download";
    return [NSTemporaryDirectory()
        stringByAppendingPathComponent:[NSString stringWithFormat:@"miniproxy-dl-%08x-%@",
                                                                  arc4random(), safe]];
}

- (void)startDownloadFromURL:(NSURL *)url suggestedName:(NSString *)name {
    NSURLSessionDownloadTask *task = [[NSURLSession sharedSession]
        downloadTaskWithRequest:[NSURLRequest requestWithURL:url]
              completionHandler:^(NSURL *tmp, NSURLResponse *response, NSError *error) {
                if (error != nil || tmp == nil) {
                    dispatch_async(dispatch_get_main_queue(), ^{
                      [self reportDownloadFailure:error];
                    });
                    return;
                }
                NSString *filename = name.length > 0 ? name : filenameFromResponse(response);
                NSString *staging = stagingPathFor(filename);
                NSError *cpErr = nil;
                if (![[NSFileManager defaultManager] copyItemAtURL:tmp
                                                             toURL:[NSURL fileURLWithPath:staging]
                                                             error:&cpErr]) {
                    dispatch_async(dispatch_get_main_queue(), ^{
                      [self reportDownloadFailure:cpErr];
                    });
                    return;
                }
                dispatch_async(dispatch_get_main_queue(), ^{
                  [self askWhereToSave:filename staged:staging];
                });
              }];
    [task resume];
}

// 用保存面板定落点：用户选了路径就等于授权，不必去碰 TCC 的「下载文件夹」权限
// （App 是 ad-hoc 签名的，TCC 记录还容易在重打包后失效）
- (void)askWhereToSave:(NSString *)filename staged:(NSString *)staging {
    NSSavePanel *panel = [NSSavePanel savePanel];
    panel.nameFieldStringValue = filename;
    panel.canCreateDirectories = YES;
    if (self.window != nil) {
        [panel beginSheetModalForWindow:self.window
                     completionHandler:^(NSModalResponse r) {
                       [self finishSave:(r == NSModalResponseOK ? panel.URL : nil) staged:staging];
                     }];
    } else {
        NSModalResponse r = [panel runModal];
        [self finishSave:(r == NSModalResponseOK ? panel.URL : nil) staged:staging];
    }
}

- (void)finishSave:(NSURL *)dst staged:(NSString *)staging {
    NSFileManager *fm = [NSFileManager defaultManager];
    if (dst == nil) {   // 用户取消
        [fm removeItemAtPath:staging error:nil];
        return;
    }
    [fm removeItemAtURL:dst error:nil];   // 覆盖同名文件（面板里已经问过了）
    NSError *err = nil;
    if (![fm moveItemAtURL:[NSURL fileURLWithPath:staging] toURL:dst error:&err]) {
        [self reportDownloadFailure:err];
    }
}

static BOOL isHttpURL(NSURL *url) {
    NSString *s = url.scheme.lowercaseString;
    return [s isEqualToString:@"http"] || [s isEqualToString:@"https"];
}

- (void)webView:(WKWebView *)webView
    decidePolicyForNavigationAction:(WKNavigationAction *)action
                    preferences:(WKWebpagePreferences *)preferences
                  decisionHandler:(void (^)(WKNavigationActionPolicy,
                                            WKWebpagePreferences *))decisionHandler {
    if (action.shouldPerformDownload) {   // <a download>（前端 downloadBytes 也走这条）
        NSURL *url = action.request.URL;
        if (isHttpURL(url)) {
            decisionHandler(WKNavigationActionPolicyCancel, preferences);
            [self startDownloadFromURL:url suggestedName:nil];
        } else {
            // blob: 之类没法用 NSURLSession，交回 WebKit 自带的下载器
            decisionHandler(WKNavigationActionPolicyDownload, preferences);
        }
        return;
    }
    decisionHandler(WKNavigationActionPolicyAllow, preferences);
}

- (void)webView:(WKWebView *)webView
    decidePolicyForNavigationResponse:(WKNavigationResponse *)response
                  decisionHandler:(void (^)(WKNavigationResponsePolicy))decisionHandler {
    // 1) application/x-x509-ca-cert（CA 证书）等 WebKit 显示不了的 MIME
    BOOL download = !response.canShowMIMEType;
    // 2) WebKit 只看 MIME，不认识 attachment —— 导出的 JSON/HAR 是 application/json，
    //    不加这一条会被当成页面把一堆文本显示出来
    NSHTTPURLResponse *http = (NSHTTPURLResponse *)response.response;
    if ([http isKindOfClass:[NSHTTPURLResponse class]]) {
        id cd = http.allHeaderFields[@"Content-Disposition"];
        if ([cd isKindOfClass:[NSString class]] &&
            [[cd lowercaseString] containsString:@"attachment"]) {
            download = YES;
        }
    }
    if (!download) {
        decisionHandler(WKNavigationResponsePolicyAllow);
        return;
    }
    NSURL *url = response.response.URL;
    if (isHttpURL(url)) {
        decisionHandler(WKNavigationResponsePolicyCancel);   // 页面别跳走，外壳自己抓
        [self startDownloadFromURL:url suggestedName:nil];
    } else {
        decisionHandler(WKNavigationResponsePolicyDownload);
    }
}

- (void)webView:(WKWebView *)webView navigationActionDidBecomeDownload:(WKDownload *)download {
    download.delegate = self;
}

- (void)webView:(WKWebView *)webView navigationResponseDidBecomeDownload:(WKDownload *)download {
    download.delegate = self;
}

#pragma mark - WKDownloadDelegate（只兜 blob: 这类非 http 的下载）

- (void)download:(WKDownload *)download
    decideDestinationUsingResponse:(NSURLResponse *)response
                 suggestedFilename:(NSString *)suggestedFilename
               completionHandler:(void (^)(NSURL *_Nullable))completionHandler {
    NSURL *dst = uniqueDestination([self downloadDirectory], suggestedFilename);
    objc_setAssociatedObject(download, kDestKey, dst, OBJC_ASSOCIATION_RETAIN);
    completionHandler(dst);
}

- (void)downloadDidFinish:(WKDownload *)download {
    NSURL *dst = objc_getAssociatedObject(download, kDestKey);
    if (dst != nil) [self reportDownloadDone:dst];
}

- (void)download:(WKDownload *)download
    didFailWithError:(NSError *)error
          resumeData:(NSData *)resumeData {
    (void)resumeData;
    [self reportDownloadFailure:error];
}

// 关闭窗口 = 退出应用（走 applicationShouldTerminate 的清理流程）
- (BOOL)windowShouldClose:(NSWindow *)sender {
    [NSApp terminate:nil];
    return NO;
}

// Cmd+Q / 关闭窗口 / Dock 退出 / SIGTERM 都会到这里
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    self.terminating = YES;
    if (self.adopted) {
        return NSTerminateNow;   // 后端不是我们的，不代管它的生命周期
    }
    // 优雅停机：让后端自己恢复系统代理、收尾在途请求
    syncRequest(@"/api/quit", @"POST", 3.0);
    for (int i = 0; i < 60 && self.child.isRunning; i++) {
        usleep(100 * 1000);   // 最多等 6 秒
    }
    return NSTerminateNow;
}

@end

static dispatch_source_t gSigSrc[3];

int main(int argc, const char **argv) {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        AppDelegate *delegate = [AppDelegate new];
        app.delegate = delegate;
        app.activationPolicy = NSApplicationActivationPolicyRegular;

        // SIGTERM/SIGINT/SIGHUP → 走与 Cmd+Q 相同的优雅退出（applicationShouldTerminate）。
        // 用 GCD signal source 在主队列上转成事件，避免在信号处理函数里碰 Objective-C。
        signal(SIGTERM, SIG_IGN);
        signal(SIGINT, SIG_IGN);
        signal(SIGHUP, SIG_IGN);
        int sigs[3] = {SIGTERM, SIGINT, SIGHUP};
        for (int i = 0; i < 3; i++) {
            gSigSrc[i] = dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, sigs[i], 0,
                                                dispatch_get_main_queue());
            dispatch_source_set_event_handler(gSigSrc[i], ^{
                [NSApp terminate:nil];
            });
            dispatch_resume(gSigSrc[i]);
        }

        [app run];
    }
    return 0;
}
