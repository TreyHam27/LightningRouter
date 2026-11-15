//
//  BackgroundService.swift
//  iTorrent
//
//  Created by Daniil Vinogradov on 05/04/2024.
//

import Foundation

protocol BackgroundServiceProtocol: Sendable {
    var isRunning: Bool { get }
    func start() -> Bool
    func stop()
    func prepare() async -> Bool
}

extension BackgroundService {
    enum Mode: String, CaseIterable, Codable, Identifiable {
        case audio = "Audio"
        case location = "Location"

        var id: Self { self }
    }
}

/// Keeps the app alive in the background, by whichever `Mode` is selected.
///
/// Main-actor bound so the strategies are created on the main thread:
/// `CLLocationManager` delivers its delegate callbacks on the run loop of the
/// thread that created it.
@MainActor
final class BackgroundService {
    private(set) var mode: Mode = .audio
    private(set) var isRunning = false

    private var impl: BackgroundServiceProtocol = AudioBackgroundService()

    @discardableResult
    func start() -> Bool {
        isRunning = impl.start()
        return isRunning
    }

    func stop() {
        isRunning = false
        impl.stop()
    }

    /// Switches to `mode`, carrying a running service over to it.
    ///
    /// False if the mode is unavailable, which leaves the current mode in
    /// place, or if the service was running and the new mode failed to start.
    func applyMode(_ mode: Mode) async -> Bool {
        guard mode != self.mode else { return await impl.prepare() }

        let next: BackgroundServiceProtocol = switch mode {
        case .audio: AudioBackgroundService()
        case .location: LocationBackgroundService()
        }
        guard await next.prepare() else { return false }

        let wasRunning = isRunning
        if wasRunning { stop() }
        impl = next
        self.mode = mode
        return wasRunning ? start() : true
    }
}
