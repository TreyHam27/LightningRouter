//
//  ContentView.swift
//  LightningRouter
//
//  Created by Jackson Coxson on 8/5/25.
//

import SwiftUI

struct ContentView: View {
    @AppStorage("tunnelMode") private var tunnelMode: Tunnel.Mode = .wireGuard
    @AppStorage("lastTargetIP") private var targetIP: String = "1.1.1.1"
    @AppStorage("lastTargetPort") private var targetPort: Int = 51820
    @AppStorage("backgroundMode") private var backgroundMode: BackgroundService.Mode = .audio
    @AppStorage("showLocationIndicator") private var showIndicator: Bool = true
    @State private var tunnel = Tunnel()

    var body: some View {
        VStack(spacing: 20) {
            Image(systemName: "globe")
                .imageScale(.large)
                .foregroundStyle(.tint)
                .font(.system(size: 40))
                .padding(.top, 20)

            Text("LightningRouter")
                .font(.title2)
                .bold()

            Picker("Mode", selection: $tunnelMode) {
                ForEach(Tunnel.Mode.allCases) { mode in
                    Text(mode.rawValue).tag(mode)
                }
            }
            .pickerStyle(.segmented)
            .padding(.horizontal)
            .disabled(tunnel.isOn)

            // The target is fixed for the life of a tunnel.
            if tunnelMode == .wireGuard {
            HStack {
                TextField("IP address or hostname", text: $targetIP)
                    .textFieldStyle(RoundedBorderTextFieldStyle())
                    .padding(.horizontal)
                    .disableAutocorrection(true)
                    .autocapitalization(.none)

                TextField("Port", value: $targetPort, format: .number.grouping(.never))
                    .textFieldStyle(RoundedBorderTextFieldStyle())
                    .padding(.horizontal)
                    .disableAutocorrection(true)
                    .autocapitalization(.none)
                    .keyboardType(.numberPad)
                    .onChange(of: targetPort) { oldPort, newPort in
                        if newPort < 1 || newPort > 65535 {
                            targetPort = oldPort
                        }
                    }
            }
            .disabled(tunnel.isOn)
            }

            Toggle("Enable Tunnel", isOn: tunnelIsOn)
                .padding(.horizontal)

            if backgroundMode == .location {
                Toggle(isOn: $showIndicator) {
                    Text("Location Indicator")
                }
                .padding(.horizontal)
            }

            Picker("Background Mode", selection: $backgroundMode) {
                ForEach(BackgroundService.Mode.allCases) { mode in
                    Text(mode.rawValue).tag(mode)
                }
            }
            .pickerStyle(.menu)
            .buttonStyle(.bordered)
            .onChange(of: backgroundMode) { oldMode, newMode in
                Task {
                    if !(await tunnel.setBackgroundMode(newMode)) {
                        backgroundMode = oldMode
                    }
                }
            }

            VStack(spacing: 6) {
                Text("Status: \(statusText)")
                    .foregroundColor(isRelaying ? .green : .secondary)
                if let problem = tunnel.problem {
                    Text(problem)
                        .foregroundColor(.red)
                }
            }
            .font(.footnote)
            .multilineTextAlignment(.center)
            .padding(.top, 10)

            Spacer()
        }
        .padding()
        .task {
            if !(await tunnel.setBackgroundMode(backgroundMode)) {
                backgroundMode = .audio
            }
        }
    }

    private var tunnelIsOn: Binding<Bool> {
        Binding {
            tunnel.isOn
        } set: { enable in
            if enable {
                switch tunnelMode {
                case .wireGuard:
                    tunnel.start(mode: .wireGuard(
                        host: targetIP.trimmingCharacters(in: .whitespaces), port: targetPort))
                case .serverless:
                    tunnel.start(mode: .serverless)
                }
            } else {
                tunnel.stop()
            }
        }
    }

    private var isRelaying: Bool {
        if case .relaying = tunnel.status { true } else { false }
    }

    private var statusText: String {
        switch tunnel.status {
        case .off:
            "Off"
        case .waitingForRelay where tunnel.mode == .serverless:
            "Waiting for the Mac"
        case .waitingForRelay:
            "Waiting for the relay on your Mac"
        case .relaying(let n, let up, let down) where tunnel.mode == .serverless:
            "Serverless: \(n) connection\(n == 1 ? "" : "s") · ↓ \(byteCount(down)) ↑ \(byteCount(up))"
        case .relaying(_, let up, let down):
            "Relaying · ↓ \(byteCount(down)) ↑ \(byteCount(up))"
        }
    }

    private func byteCount(_ bytes: UInt64) -> String {
        Int64(clamping: bytes).formatted(.byteCount(style: .binary))
    }
}

#Preview {
    ContentView()
}
