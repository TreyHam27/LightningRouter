//
//  Tunnel.swift
//  LightningRouter
//

import LightningRouterRS
import SwiftUI

/// The tunnel as the UI sees it: the Rust proxy, plus the background service
/// that keeps the app alive while the proxy runs.
@MainActor
@Observable
final class Tunnel {
    enum Status: Equatable {
        case off
        case waitingForRelay
        case relaying(up: UInt64, down: UInt64)
    }

    private(set) var status: Status = .off
    /// Why the last request failed, until the next one succeeds.
    private(set) var problem: String?

    var isOn: Bool { proxy != nil }

    private var proxy: OpaquePointer?
    private var statsPoll: Task<Void, Never>?
    private let background = BackgroundService()

    func start(host: String, port: Int) {
        guard proxy == nil else { return }
        guard let port = UInt16(exactly: port), port > 0 else {
            problem = "The port must be between 1 and 65535"
            return
        }
        guard background.start() else {
            problem = "Could not stay active in the background"
            return
        }
        var started: OpaquePointer?
        guard proxy_start(host, port, &started) else {
            background.stop()
            problem = "Could not listen for the relay"
            return
        }

        proxy = started
        problem = nil
        status = .waitingForRelay
        UIApplication.shared.isIdleTimerDisabled = true
        statsPoll = Task { [weak self] in
            while !Task.isCancelled {
                self?.refresh()
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    func stop() {
        guard let proxy else { return }
        statsPoll?.cancel()
        statsPoll = nil
        proxy_stop(proxy)
        self.proxy = nil
        background.stop()
        UIApplication.shared.isIdleTimerDisabled = false
        status = .off
    }

    /// False if the mode is unavailable, in which case the previous mode
    /// stays selected.
    func setBackgroundMode(_ mode: BackgroundService.Mode) async -> Bool {
        if await background.applyMode(mode) {
            problem = nil
            return true
        }
        problem = "\(mode.rawValue) background mode is unavailable"
        // A running tunnel that lost its background service would stall as
        // soon as the app left the foreground.
        if isOn && !background.isRunning { stop() }
        return false
    }

    private func refresh() {
        guard let proxy else { return }
        let stats = proxy_stats(proxy)
        status = stats.connections > 0
            ? .relaying(up: stats.bytes_up, down: stats.bytes_down)
            : .waitingForRelay
    }
}
