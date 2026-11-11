import Foundation

/// Owns the Rust `Disc` handle. The core is synchronous and thread-safe, so
/// each call runs on a detached task off the main actor.
@MainActor
final class DiscStore: ObservableObject {
    let disc: Disc
    @Published private(set) var datasets: [Dataset] = []

    /// - Parameter discDirectory: folder holding `disc.json`, the ZIMs and
    ///   `search-index/` (the on-demand resource / asset pack location).
    init(discDirectory: URL?) throws {
        disc = try discDirectory.map { try Disc.open(discDir: $0.path) } ?? Disc.empty()
        disc.setCacheBudget(totalBytes: UInt64(min(ProcessInfo.processInfo.physicalMemory / 32, 128 << 20)))
        datasets = disc.datasets()
    }

    func search(_ query: String, limit: UInt32 = 10) async throws -> [SearchResult] {
        let disc = self.disc
        return try await Task.detached(priority: .userInitiated) { try disc.search(query: query, limit: limit) }.value
    }

    /// Side-load a ZIM chosen with the document picker. The file is read in
    /// place; store a security-scoped bookmark and call again on next launch.
    func sideLoad(_ url: URL) async throws {
        guard url.startAccessingSecurityScopedResource() else { throw CocoaError(.fileReadNoPermission) }
        // Access stays open for as long as the archive is mounted.
        let disc = self.disc
        _ = try await Task.detached { try disc.addArchive(path: url.path) }.value
        datasets = disc.datasets()
    }
}
