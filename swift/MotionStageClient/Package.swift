// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "MotionStageClient",
    platforms: [
        .iOS(.v15),
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
