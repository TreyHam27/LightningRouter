//
//  BackgroundService.swift
//  iTorrent
//
//  Created by Daniil Vinogradov on 05/04/2024.
//

import Foundation

protocol BackgroundServiceProtocol {
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

final class BackgroundService: BackgroundServiceProtocol, @unchecked Sendable {
    @Published var isRunningPublisher: Bool = false

    public static let shared = BackgroundService()

    var isRunning: Bool { impl.isRunning }

    @discardableResult
    func start() -> Bool {
        let res = impl.start()
        isRunningPublisher = res
        return res
    }

    func stop() {
        guard isRunning else { return }
        isRunningPublisher = false
        impl.stop()
    }

    func prepare() async -> Bool {
        await impl.prepare()
    }

    func applyMode(_ mode: Mode) async -> Bool {
        print(mode)
        switch mode {
        case .audio:
            impl = AudioBackgroundService()
        case .location:
            impl = LocationBackgroundService()
        }
        return await impl.prepare()
    }

    private var impl: BackgroundServiceProtocol = AudioBackgroundService()
}
