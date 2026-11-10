// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "ConvKit",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "ConvKit",
            path: "Sources/ConvKit"
        ),
    ]
)
