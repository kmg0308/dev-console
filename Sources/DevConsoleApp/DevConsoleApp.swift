import AppKit
import DevConsoleCore
import RuntimeAtlasFeature
import SwiftUI
import TokenMeterFeature

private enum DevConsoleTab: Hashable {
    case runtimeAtlas
    case tokenMeter

    var feature: DevConsoleFeature {
        switch self {
        case .runtimeAtlas: .runtimeAtlas
        case .tokenMeter: .tokenMeter
        }
    }
}

@MainActor
private final class DevConsoleAppDelegate: NSObject, NSApplicationDelegate {
    var stopRuntimeActions: (() -> Void)?

    func applicationWillTerminate(_ notification: Notification) {
        stopRuntimeActions?()
    }
}

@MainActor
private final class DevConsoleKeyboardMonitor: ObservableObject {
    private var monitor: Any?
    private var state = DevConsoleShortcutState()
    private var cancelWorktreeSession: (() -> Void)?

    func start(
        activeFeature: @escaping () -> DevConsoleFeature,
        handle: @escaping (DevConsoleShortcutAction) -> Void,
        cancelWorktreeSession: @escaping () -> Void
    ) {
        guard monitor == nil else { return }
        self.cancelWorktreeSession = cancelWorktreeSession
        monitor = NSEvent.addLocalMonitorForEvents(
            matching: [.keyDown, .keyUp, .flagsChanged]
        ) { [weak self] event in
            guard let self else { return event }
            let shortcutEvent: DevConsoleShortcutEvent
            switch event.type {
            case .keyDown, .keyUp:
                let key: String
                switch event.keyCode {
                case 12: key = "q"
                case 48: key = "tab"
                default: return event
                }
                if event.type == .keyDown {
                    shortcutEvent = .keyDown(
                        key: key,
                        control: event.modifierFlags.contains(.control),
                        shift: event.modifierFlags.contains(.shift)
                    )
                } else {
                    shortcutEvent = .keyUp(
                        key: key,
                        control: event.modifierFlags.contains(.control)
                    )
                }
            case .flagsChanged:
                shortcutEvent = .flagsChanged(
                    control: event.modifierFlags.contains(.control)
                )
            default:
                return event
            }

            let result = DevConsoleShortcutReducer.reduce(
                self.state,
                feature: activeFeature(),
                shortcutEvent
            )
            self.state = result.state
            if let action = result.action {
                handle(action)
            }
            return result.consumed ? nil : event
        }
    }

    func stop() {
        if state.worktreeSessionIsActive {
            cancelWorktreeSession?()
        }
        state = DevConsoleShortcutState()
        cancelWorktreeSession = nil
        if let monitor {
            NSEvent.removeMonitor(monitor)
            self.monitor = nil
        }
    }
}

@MainActor
private struct DevConsoleRootView: View {
    @ObservedObject var runtimeHost: RuntimeAtlasFeatureHost
    @ObservedObject var tokenHost: TokenMeterFeatureHost
    @ObservedObject var updates: UpdateModel
    @ObservedObject var keyboard: DevConsoleKeyboardMonitor
    let appDelegate: DevConsoleAppDelegate
    @Binding var showingUpdates: Bool
    @Binding var selectedTab: DevConsoleTab

    var body: some View {
        VStack(spacing: 0) {
            Picker("Feature", selection: $selectedTab) {
                Text("Runtime Atlas").tag(DevConsoleTab.runtimeAtlas)
                Text("TokenMeter").tag(DevConsoleTab.tokenMeter)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(12)

            Divider()

            Group {
                switch selectedTab {
                case .runtimeAtlas:
                    RuntimeAtlasFeatureView(
                        host: runtimeHost,
                        keyboardMode: .hostManaged
                    )
                case .tokenMeter:
                    TokenMeterFeatureView(host: tokenHost)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .preferredColorScheme(.dark)
        .frame(
            minWidth: RuntimeAtlasTheme.minimumWindowWidth,
            minHeight: RuntimeAtlasTheme.minimumWindowHeight
        )
        .toolbar {
            if updates.release != nil {
                Button("Update Available") {
                    showingUpdates = true
                }
            }
        }
        .onAppear {
            appDelegate.stopRuntimeActions = { runtimeHost.stopAll() }
            keyboard.start(
                activeFeature: { selectedTab.feature },
                handle: handleShortcut,
                cancelWorktreeSession: { runtimeHost.cancelWorktreeSwitcher() }
            )
        }
        .onDisappear {
            keyboard.stop()
        }
        .task {
            await updates.check()
        }
        .sheet(isPresented: $showingUpdates) {
            DevConsoleUpdateView(model: updates)
        }
    }

    private func handleShortcut(_ action: DevConsoleShortcutAction) {
        switch action {
        case .switchFeature:
            selectedTab = selectedTab == .runtimeAtlas ? .tokenMeter : .runtimeAtlas
        case .switchWorktree(let forward):
            runtimeHost.advanceWorktreeSwitcher(reverse: !forward)
        case .commitWorktreeSession:
            runtimeHost.commitWorktreeSwitcher()
        }
    }
}

@MainActor
private struct DevConsoleUpdateView: View {
    @ObservedObject var model: UpdateModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("DevConsole Update")
                .font(.title2.weight(.semibold))
            Text(model.status.isEmpty ? "Check for the latest DevConsole release." : model.status)
            if let version = model.release?.version {
                Text("Available: \(version)")
            }
            if let errorMessage = model.errorMessage {
                Text(errorMessage)
                    .foregroundStyle(.red)
            }
            HStack {
                Button("Close") {
                    dismiss()
                }
                Spacer()
                Button("Check Again") {
                    Task { await model.check() }
                }
                .disabled(model.isChecking || model.isInstalling)
                if model.release != nil {
                    Button("Install and Relaunch") {
                        Task { await model.installAvailable() }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(model.isChecking || model.isInstalling)
                }
            }
        }
        .padding(24)
        .frame(width: 420)
    }
}

private struct DevConsoleCommands: Commands {
    @ObservedObject var tokenHost: TokenMeterFeatureHost
    @ObservedObject var updates: UpdateModel
    @Binding var showingUpdates: Bool
    let activeTab: DevConsoleTab

    var body: some Commands {
        CommandGroup(replacing: .newItem) {}
        CommandGroup(after: .appInfo) {
            Button("Check for Updates…") {
                showingUpdates = true
                Task { await updates.check() }
            }
        }
        CommandMenu("Data") {
            Button("Refresh TokenMeter") {
                tokenHost.refresh()
            }
            .keyboardShortcut("r", modifiers: [.command])
            .disabled(activeTab != .tokenMeter)
            Button("Rebuild Token Cache") {
                tokenHost.rebuildTokenCache()
            }
            .disabled(activeTab != .tokenMeter)
        }
    }
}

@main
@MainActor
struct DevConsoleApp: App {
    @NSApplicationDelegateAdaptor(DevConsoleAppDelegate.self) private var appDelegate
    @StateObject private var runtimeHost = RuntimeAtlasFeatureHost()
    @StateObject private var tokenHost = TokenMeterFeatureHost()
    @StateObject private var updates = UpdateModel()
    @StateObject private var keyboard = DevConsoleKeyboardMonitor()
    @State private var showingUpdates = false
    @State private var selectedTab: DevConsoleTab = .runtimeAtlas

    var body: some Scene {
        Window("DevConsole", id: "main") {
            DevConsoleRootView(
                runtimeHost: runtimeHost,
                tokenHost: tokenHost,
                updates: updates,
                keyboard: keyboard,
                appDelegate: appDelegate,
                showingUpdates: $showingUpdates,
                selectedTab: $selectedTab
            )
        }
        .windowStyle(.hiddenTitleBar)
        .commands {
            RuntimeAtlasFeatureCommands(
                host: runtimeHost,
                isEnabled: selectedTab == .runtimeAtlas
            )
            DevConsoleCommands(
                tokenHost: tokenHost,
                updates: updates,
                showingUpdates: $showingUpdates,
                activeTab: selectedTab
            )
        }

        Settings {
            RuntimeAtlasFeatureSettings(host: runtimeHost)
                .preferredColorScheme(.dark)
                .tint(RuntimeAtlasTheme.accent)
        }
    }
}
