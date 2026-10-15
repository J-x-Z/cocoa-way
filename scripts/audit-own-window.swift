// Bounded own-PID/window-only focus and input. No global posts, cursor warps,
// AppleScript, permission requests, screenshots or host/container lifecycle.
import AppKit
import ApplicationServices
import CryptoKit
import Darwin
import Foundation

struct Failure: Error { let message: String }
func fail(_ message: String) throws -> Never { throw Failure(message: message) }
func require(_ condition: Bool, _ message: String) throws {
    if !condition { try fail(message) }
}

func arguments() throws -> [String: String] {
    var result: [String: String] = [:]
    var args = Array(CommandLine.arguments.dropFirst())
    while !args.isEmpty {
        let name = args.removeFirst()
        if name == "--describe" || name == "--focus" { result[name] = "true"; continue }
        try require(!args.isEmpty, "option needs value")
        result[name] = args.removeFirst()
    }
    return result
}

func usage(_ pid: Int32) throws -> (String, UInt64) {
    var info = rusage_info_v0()
    let code = withUnsafeMutablePointer(to: &info) { pointer in
        pointer.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) {
            proc_pid_rusage(pid, RUSAGE_INFO_V0, $0)
        }
    }
    try require(code == 0, "process identity unavailable")
    let uuid = withUnsafeBytes(of: info.ri_uuid) { raw in
        raw.map { String(format: "%02x", $0) }.joined()
    }
    return (uuid, info.ri_proc_start_abstime)
}

func axValue(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}

func run() throws {
    if CommandLine.arguments.contains("--help") {
        print("audit-own-window --fixture HOST.json --describe\naudit-own-window --fixture HOST.json --focus --window-id EXACT_ID [--keys 1..100]\nRequires host_pid, host_executable, host_sha256, host_loaded_uuid, host_process_start_abstime, exclusive_probe_host=true. PID/window targeted only; never global input or permission requests.")
        return
    }
    let args = try arguments()
    guard let fixtureName = args["--fixture"] else { try fail("--fixture PATH required") }
    let fixtureURL = URL(fileURLWithPath: fixtureName)
    let fixtureSize = try fixtureURL.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
    try require(fixtureSize > 0 && fixtureSize <= 16384, "fixture exceeds 16 KiB")
    let fixtureData = try Data(contentsOf: fixtureURL)
    try require(fixtureData.count <= 16384, "fixture exceeds 16 KiB")
    guard let fixture = try JSONSerialization.jsonObject(with: fixtureData) as? [String: Any],
          let number = fixture["host_pid"] as? NSNumber,
          let path = fixture["host_executable"] as? String,
          let expectedHash = fixture["host_sha256"] as? String,
          let expectedUUID = fixture["host_loaded_uuid"] as? String,
          let expectedStart = fixture["host_process_start_abstime"] as? NSNumber,
          fixture["exclusive_probe_host"] as? Bool == true else { try fail("invalid exclusive-host fixture") }
    let pid = number.int32Value
    try require(pid > 1, "invalid host PID")
    let executableURL = URL(fileURLWithPath: path).resolvingSymlinksInPath()
    let size = try executableURL.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
    try require(size > 0 && size <= 128 * 1024 * 1024, "executable size invalid")
    let bytes = try Data(contentsOf: executableURL)
    let actualHash = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    try require(actualHash == expectedHash, "executable SHA256 mismatch")
    func verify() throws {
        var buffer = [CChar](repeating: 0, count: 4096)
        try require(proc_pidpath(pid, &buffer, UInt32(buffer.count)) > 0, "PID path unavailable")
        let actualURL = URL(fileURLWithPath: String(cString: buffer)).resolvingSymlinksInPath()
        try require(actualURL == executableURL, "PID executable path mismatch")
        let identity = try usage(pid)
        try require(identity.0 == expectedUUID && identity.1 == expectedStart.uint64Value, "PID identity changed")
    }
    try verify()
    let rows = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as? [[String: Any]] ?? []
    let owned = rows.filter {
        ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == pid &&
        ($0[kCGWindowLayer as String] as? NSNumber)?.intValue == 0
    }.compactMap { row -> [String: Any]? in
        guard let window = row[kCGWindowNumber as String] as? NSNumber,
              let bounds = row[kCGWindowBounds as String] as? NSDictionary,
              let rect = CGRect(dictionaryRepresentation: bounds), rect.width > 100, rect.height > 100 else { return nil }
        return ["window_id": window, "title": row[kCGWindowName as String] as? String ?? "",
                "x": rect.minX, "y": rect.minY, "width": rect.width, "height": rect.height]
    }
    var report: [String: Any] = ["schema": 1, "pid": pid, "windows": owned,
        "posting_access": CGPreflightPostEventAccess(), "accessibility_trusted": AXIsProcessTrusted(),
        "global_events_posted": false, "visible_or_physical_validation": false,
        "bundle_identifier": NSRunningApplication(processIdentifier: pid)?.bundleIdentifier ?? ""]
    if args["--focus"] == "true" {
        let count = Int(args["--keys"] ?? "20") ?? 0
        try require((1...100).contains(count), "key count must be 1..100")
        guard let text = args["--window-id"], let windowID = UInt32(text),
              owned.filter({ ($0["window_id"] as? NSNumber)?.uint32Value == windowID }).count == 1,
              let target = owned.first(where: { ($0["window_id"] as? NSNumber)?.uint32Value == windowID }),
              target["title"] as? String == fixture["expected_window_title"] as? String else {
            try fail("focus requires exact ID and expected title of an owned normal window")
        }
        try require(CGPreflightPostEventAccess(), "posting access unavailable; no permission prompt requested")
        try verify()
        report["activation_requested"] = NSRunningApplication(processIdentifier: pid)?.activate(options: []) ?? false
        guard let x = target["x"] as? CGFloat, let y = target["y"] as? CGFloat,
              let width = target["width"] as? CGFloat, let height = target["height"] as? CGFloat else {
            try fail("owned window bounds unavailable")
        }
        let app = AXUIElementCreateApplication(pid)
        if AXIsProcessTrusted(), let windows = axValue(app, kAXWindowsAttribute) as? [AXUIElement] {
            let matching = windows.filter { element in
                guard let position = axValue(element, kAXPositionAttribute), let size = axValue(element, kAXSizeAttribute),
                      CFGetTypeID(position) == AXValueGetTypeID(), CFGetTypeID(size) == AXValueGetTypeID() else { return false }
                var point = CGPoint.zero, dimensions = CGSize.zero
                guard AXValueGetValue(unsafeBitCast(position, to: AXValue.self), .cgPoint, &point),
                      AXValueGetValue(unsafeBitCast(size, to: AXValue.self), .cgSize, &dimensions) else { return false }
                return abs(point.x - x) < 2 && abs(point.y - y) < 2 && abs(dimensions.width - width) < 2 && abs(dimensions.height - height) < 2
            }
            if matching.count == 1 {
                report["AX_raise_code"] = AXUIElementPerformAction(matching[0], kAXRaiseAction as CFString).rawValue
                report["AX_focus_code"] = AXUIElementSetAttributeValue(app, kAXFocusedWindowAttribute as CFString, matching[0]).rawValue
            }
        }
        guard let source = CGEventSource(stateID: .privateState) else { try fail("private event source unavailable") }
        let point = CGPoint(x: x + width / 2, y: y + height / 2)
        var postedWindowNumbers: [Int] = []
        for (index, kind): (Int, NSEvent.EventType) in [.mouseMoved, .leftMouseDown, .leftMouseUp].enumerated() {
            try verify()
            guard let nativeEvent = NSEvent.mouseEvent(with: kind, location: NSPoint(x: width / 2, y: height / 2),
                    modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: Int(windowID),
                    context: nil, eventNumber: index + 1, clickCount: kind == .mouseMoved ? 0 : 1,
                    pressure: kind == .leftMouseDown ? 1 : 0),
                  let event = nativeEvent.cgEvent else { try fail("window-targeted mouse allocation failed") }
            try require(nativeEvent.windowNumber == Int(windowID), "AppKit mouse window does not match owned ID")
            event.flags = []
            event.location = point
            event.setIntegerValueField(CGEventField(rawValue: 91)!, value: Int64(windowID))
            event.setIntegerValueField(CGEventField(rawValue: 92)!, value: Int64(windowID))
            postedWindowNumbers.append(NSEvent(cgEvent: event)?.windowNumber ?? -1)
            event.postToPid(pid)
            Thread.sleep(forTimeInterval: 0.05)
        }
        Thread.sleep(forTimeInterval: 0.2)
        for _ in 0..<count {
            for down in [true, false] {
                try verify()
                guard let event = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down) else { try fail("key allocation failed") }
                event.flags = []
                event.postToPid(pid)
                Thread.sleep(forTimeInterval: down ? 0.03 : 0.12)
            }
        }
        report["target_window_id"] = windowID
        report["key_pairs_posted"] = count
        report["targeted_pointer_events_posted"] = 3
        report["posted_mouse_window_numbers"] = postedWindowNumbers
        report["guest_receipt_proven_by_posting"] = false
    }
    let data = try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
}

do { try run() } catch {
    fputs("Own-window audit refused: \(error)\n", stderr)
    exit(1)
}
