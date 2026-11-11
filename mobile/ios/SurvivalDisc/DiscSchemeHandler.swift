import Foundation
import WebKit

/// Serves `survival-disc://<dataset-id>/<path>` from the Rust core.
final class DiscSchemeHandler: NSObject, WKURLSchemeHandler {
    static let scheme = "survival-disc"

    private let disc: Disc
    private let options: () -> ReaderOptions
    private let queue = DispatchQueue(label: "disc.resources", qos: .userInitiated, attributes: .concurrent)
    private let lock = NSLock()
    private var active = Set<ObjectIdentifier>()

    init(disc: Disc, options: @escaping () -> ReaderOptions) {
        self.disc = disc
        self.options = options
    }

    static func url(datasetId: String, urlPath: String) -> URL? {
        URL(string: "\(scheme)://\(datasetId)/\(urlPath)")
    }

    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        let id = ObjectIdentifier(task)
        lock.withLock { _ = active.insert(id) }
        let options = self.options()
        queue.async { [self] in
            guard let url = task.request.url, let dataset = url.host,
                  let comps = URLComponents(url: url, resolvingAgainstBaseURL: false) else {
                return finish(task, id, status: 400, mime: "text/plain", body: Data())
            }
            var path = String(comps.percentEncodedPath.dropFirst())
            if let q = comps.percentEncodedQuery { path += "?\(q)" }
            do {
                let r = try disc.getResource(datasetId: dataset, urlPath: path, options: options)
                finish(task, id, status: 200, mime: r.mime, body: r.data)
            } catch DiscError.NotFound {
                let html = disc.messagePage(title: "Not in this disc",
                                            message: "This page is not part of the installed library.",
                                            options: options)
                finish(task, id, status: 404, mime: "text/html; charset=utf-8", body: Data(html.utf8))
            } catch {
                finish(task, id, status: 403, mime: "text/plain", body: Data())
            }
        }
    }

    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {
        lock.withLock { _ = active.remove(ObjectIdentifier(task)) }
    }

    /// WebKit raises if a stopped task is messaged, so delivery is serialised
    /// on the main thread and gated on the active set.
    private func finish(_ task: WKURLSchemeTask, _ id: ObjectIdentifier, status: Int, mime: String, body: Data) {
        DispatchQueue.main.async { [self] in
            guard lock.withLock({ active.remove(id) != nil }), let url = task.request.url,
                  let response = HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1",
                                                 headerFields: ["Content-Type": mime,
                                                                "Content-Length": String(body.count)]) else { return }
            task.didReceive(response)
            task.didReceive(body)
            task.didFinish()
        }
    }
}
