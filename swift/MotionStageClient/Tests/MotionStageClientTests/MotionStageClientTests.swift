import XCTest
@testable import MotionStageClient

final class MotionStageClientTests: XCTestCase {
    func testRuntimeModeRawValuesRemainStable() {
        XCTAssertEqual(RuntimeMode.idle.rawValue, 0)
        XCTAssertEqual(RuntimeMode.live.rawValue, 1)
        XCTAssertEqual(RuntimeMode.recording.rawValue, 2)
    }
}
