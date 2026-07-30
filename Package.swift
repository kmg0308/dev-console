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
            revision: "caa9fae88153de439efa0fb29587e63ce4e87a6c"
        ),
        .package(
            url: "https://github.com/kmg0308/token-scope.git",
            revision: "a4059242384e3ce429406dfa1bb5e16c06667c7b"
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
