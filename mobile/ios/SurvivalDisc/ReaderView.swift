import SwiftUI
import WebKit

/// Locked-down article view: no JavaScript, no persistent storage, and only
/// the disc scheme may navigate.
struct ReaderView: UIViewRepresentable {
    let disc: Disc
    let url: URL
    let options: ReaderOptions
    var onExternalLink: (URL) -> Void = { _ in }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        config.setURLSchemeHandler(
            DiscSchemeHandler(disc: disc, options: { [coordinator = context.coordinator] in coordinator.parent.options }),
            forURLScheme: DiscSchemeHandler.scheme)
        let view = WKWebView(frame: .zero, configuration: config)
        view.navigationDelegate = context.coordinator
        view.allowsLinkPreview = false
        return view
    }

    func updateUIView(_ view: WKWebView, context: Context) {
        let optionsChanged = context.coordinator.parent.options != options
        context.coordinator.parent = self
        if view.url != url {
            view.load(URLRequest(url: url))
        } else if optionsChanged {
            view.reload()
        }
    }

    final class Coordinator: NSObject, WKNavigationDelegate {
        var parent: ReaderView
        init(_ parent: ReaderView) { self.parent = parent }

        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction) async -> WKNavigationActionPolicy {
            guard let url = action.request.url else { return .cancel }
            if url.scheme == DiscSchemeHandler.scheme { return .allow }
            parent.onExternalLink(url)
            return .cancel
        }
    }
}
