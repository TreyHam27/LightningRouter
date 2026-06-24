//
//  ContentView.swift
//  LightningRouter
//
//  Created by Jackson Coxson on 8/5/25.
//

import SwiftUI
import LightningRouterRS

struct ContentView: View {
    @State private var plsEnable = false
    @AppStorage("lastTargetIP") private var targetIP: String = "1.1.1.1"
    @AppStorage("lastTargetPort") private var targetPort: Int = 51820
    @AppStorage("backgroundMode") var backgroundMode: BackgroundService.Mode = .audio
    @AppStorage("showLocationIndicator") var showIndicator: Bool = true
    @State var backgroundService: BackgroundService = BackgroundService()
    @State private var threadKiller: OpaquePointer? = nil
    @State private var connectionStatus: String = "Disconnected"

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

            HStack {
                TextField("IP address", text: $targetIP)
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

            Toggle(isOn: $plsEnable) {
                Text("Enable Tunnel")
            }
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
                    if !(await backgroundService.applyMode(newMode)) {
                        connectionStatus = "Error: Failed to set background mode"
                    }
                }
            }

            Text("Status: \(connectionStatus)")
                .foregroundColor(plsEnable ? .green : .secondary)
                .font(.footnote)
                .padding(.top, 10)

            Spacer()
        }
        .onAppear {
            Task {
                await backgroundService.applyMode(backgroundMode)
            }
        }
        .padding()
        .onChange(of: plsEnable) { oldValue, newValue in
            print("Tunnel changed: \(newValue), IP: \(targetIP)")

            if threadKiller != nil {
                thread_killer_kill(threadKiller)
                threadKiller = nil
                connectionStatus = "Disconnected"
                UIApplication.shared.isIdleTimerDisabled = false
                if backgroundService.isRunning { backgroundService.stop() }
            }

            if newValue {
                if backgroundService.start() {
                    let cAddress = strdup(targetIP)
                    let success = start_simple_udp_proxy(cAddress, UInt16(targetPort), &threadKiller)
                    free(cAddress)

                    if success {
                        connectionStatus = "Connected"
                        UIApplication.shared.isIdleTimerDisabled = true
                    } else {
                        connectionStatus = "Error: Failed to connect"
                        plsEnable = false
                    }
                }
            }
        }
    }
}

#Preview {
    ContentView()
}
