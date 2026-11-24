import UIKit

private final class GPUITextPosition: UITextPosition {
    let offset: Int
    init(_ offset: Int) { self.offset = offset }
}

private final class GPUITextRange: UITextRange {
    let value: NSRange
    init(_ value: NSRange) { self.value = value }
    override var start: UITextPosition { GPUITextPosition(value.location) }
    override var end: UITextPosition { GPUITextPosition(NSMaxRange(value)) }
    override var isEmpty: Bool { value.length == 0 }
}

final class GPUITextInput: UIView, UITextInput {
    weak var keyEventTarget: GPUIHostView?
    weak var inputDelegate: UITextInputDelegate?
    var markedTextStyle: [NSAttributedString.Key: Any]?
    lazy var tokenizer: UITextInputTokenizer = UITextInputStringTokenizer(textInput: self)
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var inlinePredictionType: UITextInlinePredictionType = .no
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .default
    var keyboardAppearance: UIKeyboardAppearance = .default
    var returnKeyType: UIReturnKeyType = .default
    var enablesReturnKeyAutomatically = false
    var isSecureTextEntry = false
    private var floatingCursorOffset: CGPoint?
    private var pasteGeneration: UInt64 = 0
    private var inputOwner: UInt64 = 0

    func setInputOwner(_ owner: UInt64) {
        if inputOwner != owner {
            inputOwner = owner
            pasteGeneration &+= 1
        }
    }
    private var editing = false
    private var servingOldSnapshot = false
    private var synchronizationPending = false
    private var snapshot: Snapshot?
    private var framePending = false

    private struct Snapshot: Equatable {
        var text: String
        var selection: NSRange
        var marked: NSRange

        static func == (left: Self, right: Self) -> Bool {
            left.text.utf16.elementsEqual(right.text.utf16)
                && left.selection == right.selection && left.marked == right.marked
        }
    }

    override var canBecomeFirstResponder: Bool { true }
    override func point(inside point: CGPoint, with event: UIEvent?) -> Bool { false }

    override func resignFirstResponder() -> Bool {
        pasteGeneration &+= 1
        unmarkText()
        return super.resignFirstResponder()
    }

    private func nativeRange(_ kind: UInt32) -> NSRange {
        let value = gpui_ios_text_range(kind)
        return NSRange(location: value.location, length: value.length)
    }

    private func bridge(_ range: NSRange) -> TextRange {
        TextRange(location: range.location, length: range.length)
    }

    private func read(_ range: NSRange) -> String? {
        guard range.location != NSNotFound else { return nil }
        let count = gpui_ios_read_text(bridge(range), nil, 0)
        guard count != NSNotFound else { return nil }
        if count == 0 { return "" }
        var bytes = [UInt8](repeating: 0, count: count)
        let written = bytes.withUnsafeMutableBufferPointer {
            gpui_ios_read_text(bridge(range), $0.baseAddress, $0.count)
        }
        guard written <= count else { return nil }
        return String(bytes: bytes.prefix(written), encoding: .utf8)
    }

    private func liveSnapshot() -> Snapshot {
        let document = nativeRange(2)
        return Snapshot(text: read(document) ?? "", selection: nativeRange(0), marked: nativeRange(1))
    }

    func scheduleSynchronization() {
        guard isFirstResponder, !synchronizationPending else { return }
        synchronizationPending = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.synchronizationPending = false
            if self.isFirstResponder { self.synchronize() }
        }
    }

    func synchronize() {
        guard !editing, !servingOldSnapshot else { return }
        let current = liveSnapshot()
        guard snapshot != current else { return }
        let textChanged = snapshot.map { !$0.text.utf16.elementsEqual(current.text.utf16) } ?? true
            || snapshot?.marked != current.marked
        let selectionChanged = snapshot?.selection != current.selection
        // UIKit's will-change queries must still see the previous document.
        servingOldSnapshot = true
        if textChanged { inputDelegate?.textWillChange(self) }
        if selectionChanged { inputDelegate?.selectionWillChange(self) }
        snapshot = current
        servingOldSnapshot = false
        if textChanged { inputDelegate?.textDidChange(self) }
        if selectionChanged { inputDelegate?.selectionDidChange(self) }
    }

    private func edit(_ body: () -> Void) {
        guard !editing, !servingOldSnapshot else { return }
        // Report any GPUI edits before adopting the result of this UIKit edit.
        // did-change callbacks may edit once the new snapshot is being served.
        synchronize()
        editing = true
        body()
        snapshot = liveSnapshot()
        editing = false
        presentAfterEdits()
    }

    // One keystroke can be several UIKit edits in the same run-loop turn: Backspace
    // selects the previous character, then deletes it. Present only their result,
    // without waiting for the display link's next tick.
    private func presentAfterEdits() {
        guard !framePending else { return }
        framePending = true
        RunLoop.main.perform(inModes: [.common]) { [weak self] in
            self?.framePending = false
            _ = gpui_ios_request_frame()
        }
    }

    private var documentLength: Int {
        servingOldSnapshot ? (snapshot?.text.utf16.count ?? 0) : nativeRange(2).length
    }
    var hasText: Bool { documentLength > 0 }
    var beginningOfDocument: UITextPosition { GPUITextPosition(0) }
    var endOfDocument: UITextPosition {
        GPUITextPosition(documentLength)
    }
    var selectedTextRange: UITextRange? {
        get {
            let range = servingOldSnapshot ? snapshot?.selection ?? NSRange(location: NSNotFound, length: 0) : nativeRange(0)
            return range.location == NSNotFound ? nil : GPUITextRange(range)
        }
        set {
            guard let range = newValue as? GPUITextRange else { return }
            edit { gpui_ios_select_text(bridge(range.value)) }
        }
    }
    var markedTextRange: UITextRange? {
        let range = servingOldSnapshot ? snapshot?.marked ?? NSRange(location: NSNotFound, length: 0) : nativeRange(1)
        return range.location == NSNotFound ? nil : GPUITextRange(range)
    }

    func text(in range: UITextRange) -> String? {
        guard let range = range as? GPUITextRange else { return nil }
        if servingOldSnapshot {
            let text = (snapshot?.text ?? "") as NSString
            guard NSMaxRange(range.value) <= text.length else { return nil }
            return text.substring(with: range.value)
        }
        return read(range.value)
    }

    func replace(_ range: UITextRange, withText text: String) {
        guard let range = range as? GPUITextRange else { return }
        replace(range.value, text: text, marked: false, selection: NSRange(location: NSNotFound, length: 0))
    }

    private func replace(_ range: NSRange, text: String, marked: Bool, selection: NSRange) {
        edit {
            withUTF8(text) { bytes, length in
                gpui_ios_replace_text(bridge(range), bytes, length, marked ? 1 : 0, bridge(selection))
            }
        }
    }

    func insertText(_ text: String) {
        if text == "\n" {
            sendKey(name: "enter", character: "\n", modifiers: 0, down: true, repeatKey: false)
            sendKey(name: "enter", character: "\n", modifiers: 0, down: false, repeatKey: false)
            synchronize()
        } else {
            replace(NSRange(location: NSNotFound, length: 0), text: text, marked: false,
                    selection: NSRange(location: NSNotFound, length: 0))
        }
    }

    override var keyCommands: [UIKeyCommand]? {
        [UIKeyCommand(input: "v", modifierFlags: .command, action: #selector(paste(_:)))]
    }

    override func paste(_ sender: Any?) {
        paste(itemProviders: UIPasteboard.general.itemProviders)
    }

    override func paste(itemProviders: [NSItemProvider]) {
        guard let provider = itemProviders.first(where: { $0.canLoadObject(ofClass: NSString.self) }) else { return }
        let expected = liveSnapshot()
        let generation = pasteGeneration
        provider.loadObject(ofClass: NSString.self) { [weak self] value, _ in
            guard let text = value as? String else { return }
            DispatchQueue.main.async {
                guard let self, self.isFirstResponder, self.pasteGeneration == generation,
                      self.liveSnapshot() == expected else { return }
                self.edit { withUTF8(text) { gpui_ios_paste_text($0, $1) } }
            }
        }
    }

    func deleteBackward() { edit { gpui_ios_delete_backward() } }
    func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        replace(NSRange(location: NSNotFound, length: 0), text: markedText ?? "", marked: true, selection: selectedRange)
    }
    func unmarkText() { edit { gpui_ios_unmark_text() } }

    func textRange(from fromPosition: UITextPosition, to toPosition: UITextPosition) -> UITextRange? {
        guard let from = fromPosition as? GPUITextPosition, let to = toPosition as? GPUITextPosition else { return nil }
        return GPUITextRange(NSRange(location: min(from.offset, to.offset), length: abs(to.offset - from.offset)))
    }
    func position(from position: UITextPosition, offset: Int) -> UITextPosition? {
        guard let position = position as? GPUITextPosition else { return nil }
        let (target, overflow) = position.offset.addingReportingOverflow(offset)
        guard !overflow, target >= 0, target <= documentLength else { return nil }
        return GPUITextPosition(target)
    }
    func position(from position: UITextPosition, in direction: UITextLayoutDirection, offset: Int) -> UITextPosition? {
        switch direction {
        case .left: return self.position(from: position, offset: -offset)
        case .right: return self.position(from: position, offset: offset)
        case .up, .down:
            let rect = caretRect(for: position)
            return closestPosition(to: CGPoint(x: rect.midX, y: rect.midY + CGFloat(direction == .up ? -offset : offset) * rect.height))
        @unknown default: return nil
        }
    }
    func compare(_ position: UITextPosition, to other: UITextPosition) -> ComparisonResult {
        let a = (position as? GPUITextPosition)?.offset ?? 0
        let b = (other as? GPUITextPosition)?.offset ?? 0
        return a == b ? .orderedSame : (a < b ? .orderedAscending : .orderedDescending)
    }
    func offset(from: UITextPosition, to toPosition: UITextPosition) -> Int {
        ((toPosition as? GPUITextPosition)?.offset ?? 0) - ((from as? GPUITextPosition)?.offset ?? 0)
    }
    func position(within range: UITextRange, farthestIn direction: UITextLayoutDirection) -> UITextPosition? {
        direction == .left || direction == .up ? range.start : range.end
    }
    func characterRange(byExtending position: UITextPosition, in direction: UITextLayoutDirection) -> UITextRange? {
        guard let position = position as? GPUITextPosition else { return nil }
        if direction == .up || direction == .down {
            guard let next = self.position(from: position, in: direction, offset: 1) else { return nil }
            return textRange(from: position, to: next)
        }
        let document = GPUITextRange(NSRange(location: 0, length: documentLength))
        guard let text = text(in: document) as NSString? else { return nil }
        let index = direction == .left ? position.offset - 1 : position.offset
        guard index >= 0, index < text.length else { return nil }
        return GPUITextRange(text.rangeOfComposedCharacterSequence(at: index))
    }
    func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection { .natural }
    func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {}
    func firstRect(for range: UITextRange) -> CGRect {
        guard let range = range as? GPUITextRange else { return .zero }
        let rect = gpui_ios_text_rect(bridge(range.value))
        return CGRect(x: rect.x, y: rect.y, width: rect.width, height: rect.height)
    }
    func caretRect(for position: UITextPosition) -> CGRect {
        guard let position = position as? GPUITextPosition else { return .zero }
        var rect = firstRect(for: GPUITextRange(NSRange(location: position.offset, length: 0)))
        rect.size.width = max(1, rect.width)
        return rect
    }
    func selectionRects(for range: UITextRange) -> [UITextSelectionRect] { [] }
    func closestPosition(to point: CGPoint) -> UITextPosition? {
        let index = gpui_ios_character_at_point(point.x, point.y)
        return index == NSNotFound ? nil : GPUITextPosition(index)
    }
    func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? {
        guard let position = closestPosition(to: point) as? GPUITextPosition,
              let range = range as? GPUITextRange else { return nil }
        return GPUITextPosition(min(max(position.offset, range.value.location), NSMaxRange(range.value)))
    }
    func characterRange(at point: CGPoint) -> UITextRange? {
        guard let position = closestPosition(to: point) else { return nil }
        return tokenizer.rangeEnclosingPosition(position, with: .character, inDirection: UITextDirection.storage(.forward))
    }

    // Keep the drag relative to the caret, so it does not jump when the spacebar is first held.
    func beginFloatingCursor(at point: CGPoint) {
        guard let caret = selectedTextRange?.end else { return }
        let rect = caretRect(for: caret)
        floatingCursorOffset = CGPoint(x: rect.midX - point.x, y: rect.midY - point.y)
    }
    func updateFloatingCursor(at point: CGPoint) {
        guard let offset = floatingCursorOffset else { return }
        edit { gpui_ios_move_caret_toward(point.x + offset.x, point.y + offset.y) }
    }
    func endFloatingCursor() { floatingCursorOffset = nil }

    private func unhandledPresses(_ presses: Set<UIPress>, down: Bool, repeatKey: Bool) -> Set<UIPress> {
        // Candidate navigation and confirmation belong to UIKit during composition.
        let composing = markedTextRange == nil ? Set<UIPress>() : Set(presses.filter {
            guard let key = $0.key else { return true }
            return key.modifierFlags.intersection([.command, .control]).isEmpty
        })
        // UIKit's standard Paste command carries the user's clipboard intent.
        let pasting = Set(presses.filter {
            $0.key?.modifierFlags.contains(.command) == true
                && $0.key?.charactersIgnoringModifiers.lowercased() == "v"
        })
        let raw = presses.subtracting(composing).subtracting(pasting)
        return composing.union(pasting).union(keyEventTarget?.sendPresses(raw, down: down, repeatKey: repeatKey) ?? raw)
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let remaining = unhandledPresses(presses, down: true, repeatKey: false)
        if !remaining.isEmpty { super.pressesBegan(remaining, with: event) }
    }
    override func pressesChanged(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let remaining = unhandledPresses(presses, down: true, repeatKey: true)
        if !remaining.isEmpty { super.pressesChanged(remaining, with: event) }
    }
    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let remaining = unhandledPresses(presses, down: false, repeatKey: false)
        if !remaining.isEmpty { super.pressesEnded(remaining, with: event) }
    }
    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let remaining = unhandledPresses(presses, down: false, repeatKey: false)
        if !remaining.isEmpty { super.pressesCancelled(remaining, with: event) }
    }
}

@_cdecl("gpui_ios_host_text_input_changed")
public func gpuiHostTextInputChanged() {
    GPUIHostBridge.view?.keyboardProxy.scheduleSynchronization()
}
