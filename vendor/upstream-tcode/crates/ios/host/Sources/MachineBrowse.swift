import Foundation
import Network

@_silgen_name("tcode_ios_browse_found")
private func browseFound(
    _ request: UInt64,
    _ idBytes: UnsafePointer<UInt8>?,
    _ idLength: Int,
    _ addressBytes: UnsafePointer<UInt8>?,
    _ addressLength: Int
)

/// One DNS-SD browse for paired machines, run by the system's Bonjour daemon
/// so no multicast entitlement is needed. Each `_tcode._udp` instance is
/// resolved to an `ip:port` by opening a UDP connection to its endpoint,
/// which is ready once the path is known and is closed at once; the TXT id
/// travels back with the address and the Rust side keeps only the machine it
/// asked for.
private final class MachineBrowse {
    /// Main-queue only.
    static var pending: [UInt64: MachineBrowse] = [:]
    private static let maximumResolves = 32

    private let request: UInt64
    private let queue = DispatchQueue(label: "com.tryanks.tcode.lan-browse")
    private let browser = NWBrowser(
        for: .bonjourWithTXTRecord(type: "_tcode._udp", domain: nil),
        using: .udp
    )
    private var resolving: Set<NWEndpoint> = []
    private var connections: [NWConnection] = []

    init(_ request: UInt64) {
        self.request = request
    }

    func start() {
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            self?.resolve(results)
        }
        browser.start(queue: queue)
    }

    func stop() {
        queue.async { [self] in
            browser.cancel()
            connections.forEach { $0.cancel() }
            connections.removeAll()
        }
    }

    private func resolve(_ results: Set<NWBrowser.Result>) {
        for result in results {
            guard case .bonjour(let txt) = result.metadata,
                  txt["v"] == "1",
                  let id = txt["id"], id.utf8.count == 64,
                  !resolving.contains(result.endpoint),
                  resolving.count < Self.maximumResolves
            else { continue }
            resolving.insert(result.endpoint)
            let connection = NWConnection(to: result.endpoint, using: .udp)
            connection.stateUpdateHandler = { [weak self, weak connection] state in
                guard let self, let connection else { return }
                switch state {
                case .ready:
                    if case .hostPort(let host, let port)? = connection.currentPath?.remoteEndpoint {
                        deliver(id: id, host: host, port: port)
                    }
                    connection.cancel()
                case .failed, .cancelled:
                    connections.removeAll { $0 === connection }
                default:
                    break
                }
            }
            connections.append(connection)
            connection.start(queue: queue)
        }
    }

    private func deliver(id: String, host: NWEndpoint.Host, port: NWEndpoint.Port) {
        let address: String
        switch host {
        case .ipv4(let ipv4):
            address = "\(ipv4):\(port.rawValue)"
        case .ipv6(let ipv6):
            // A scoped address carries `%interface`; the Rust side drops
            // link-local addresses anyway.
            let bare = "\(ipv6)".split(separator: "%").first.map(String.init) ?? ""
            address = "[\(bare)]:\(port.rawValue)"
        default:
            return
        }
        withUTF8(id) { idBytes, idLength in
            withUTF8(address) { addressBytes, addressLength in
                browseFound(request, idBytes, idLength, addressBytes, addressLength)
            }
        }
    }
}

@_cdecl("tcode_ios_host_browse_start")
public func tcodeIosHostBrowseStart(_ request: UInt64) {
    DispatchQueue.main.async {
        let browse = MachineBrowse(request)
        MachineBrowse.pending[request] = browse
        browse.start()
    }
}

@_cdecl("tcode_ios_host_browse_stop")
public func tcodeIosHostBrowseStop(_ request: UInt64) {
    DispatchQueue.main.async {
        MachineBrowse.pending.removeValue(forKey: request)?.stop()
    }
}
