// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "DevConsole",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "DevConsoleCore", targets: ["DevConsoleCore"]),
        .executable(name: "DevConsole", targets: ["DevConsoleApp"]),
        .executable(name: "DevConsoleSelfTest", targets: ["DevConsoleSelfTest"]),
        .executable(
            name: "DevConsoleRuntimeAtlasCLI",
            targets: ["DevConsoleRuntimeAtlasCLI"]
        ),
        .executable(
            name: "DevConsoleRuntimeAtlasSupervisor",
            targets: ["DevConsoleRuntimeAtlasSupervisor"]
        )
    ],
    dependencies: [
        .package(
            url: "https://github.com/kmg0308/runtime_atlas.git",
            revision: "59702ca141f43d123e22e14c27b0df2bca33d2f0"
        ),
        .package(
            url: "https://github.com/kmg0308/token-scope.git",
            revision: "4980c27572462d01be7f36616cf43d365f539479"
        )
    ],
    targets: [
        .target(name: "DevConsoleCore"),
        .executableTarget(
            name: "DevConsoleApp",
            dependencies: [
                "DevConsoleCore",
                .product(name: "RuntimeAtlasFeature", package: "runtime_atlas"),
                .product(name: "TokenMeterFeature", package: "token-scope")
            ]
        ),
        .executableTarget(name: "DevConsoleSelfTest", dependencies: ["DevConsoleCore"]),
        .executableTarget(
            name: "DevConsoleRuntimeAtlasCLI",
            dependencies: [
                .product(name: "RuntimeAtlasCommandLine", package: "runtime_atlas")
            ]
        ),
        .executableTarget(
            name: "DevConsoleRuntimeAtlasSupervisor",
            dependencies: [
                .product(name: "RuntimeAtlasSupervisorCore", package: "runtime_atlas")
            ]
        )
    ]
)
