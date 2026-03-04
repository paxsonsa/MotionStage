// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "MotionStageClient",
    platforms: [
        .iOS(.v26),
    ],
    products: [
        .library(
            name: "MotionStageClient",
            targets: ["MotionStageClient"]
        ),
    ],
    targets: [
        .binaryTarget(
            name: "MotionStageSwiftFFI",
            path: "Artifacts/MotionStageSwiftFFI.xcframework"
        ),
        .target(
            name: "MotionStageClient",
            dependencies: ["MotionStageSwiftFFI"]
        ),
        .testTarget(
            name: "MotionStageClientTests",
            dependencies: ["MotionStageClient"]
        ),
    ]
)
