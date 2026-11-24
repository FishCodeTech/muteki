import QuartzCore
import UIKit

final class GPUIHostViewController: UIViewController {
    private var started = false
    private var displayLink: CADisplayLink?
    private var framesSuspended = false
    private weak var frameScreen: UIScreen?
    private weak var frameScene: UIWindowScene?
    private var appBackgroundDark: Bool?
    private var launchCover: UIView?

    override var preferredStatusBarStyle: UIStatusBarStyle {
        let dark = appBackgroundDark
            ?? (traitCollection.userInterfaceStyle == .dark)
        return dark ? .lightContent : .darkContent
    }

    override var supportedInterfaceOrientations: UIInterfaceOrientationMask {
        traitCollection.userInterfaceIdiom == .pad
            ? .all
            : [.portrait, .landscapeLeft, .landscapeRight]
    }

    private var hostView: GPUIHostView {
        view as! GPUIHostView
    }

    override func loadView() {
        view = GPUIHostView(frame: .zero)
        GPUIHostBridge.controller = self
        let cover = UIView(frame: view.bounds)
        cover.backgroundColor = UIColor(named: "LaunchBackground")
        cover.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        let glyph = UIImageView(image: UIImage(named: "LaunchGlyph"))
        glyph.translatesAutoresizingMaskIntoConstraints = false
        glyph.contentMode = .scaleAspectFit
        cover.addSubview(glyph)
        NSLayoutConstraint.activate([
            glyph.centerXAnchor.constraint(equalTo: cover.centerXAnchor),
            glyph.centerYAnchor.constraint(equalTo: cover.centerYAnchor),
            glyph.widthAnchor.constraint(equalToConstant: 96),
            glyph.heightAnchor.constraint(equalToConstant: 96),
        ])
        view.addSubview(cover)
        launchCover = cover
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        startGPUIIfNeeded()
    }

    func startGPUIIfNeeded() {
        guard !started else { return }
        view.layoutIfNeeded()
        guard hostView.attachToGPUI() else { return }

        started = true
        gpui_ios_init()
        tcode_ios_start()

        updateScreen()
        wakeFrames()
    }

    func updateScreen() {
        guard let scene = hostView.window?.windowScene else {
            displayLink?.isPaused = true
            return
        }
        let screen = scene.screen
        gpui_ios_display_changed(Double(screen.maximumFramesPerSecond))
        guard started else { return }
        if frameScreen !== screen || frameScene !== scene {
            displayLink?.invalidate()
            displayLink = scene.displayLink(target: self, selector: #selector(renderFrame))
            frameScreen = screen
            frameScene = scene
            displayLink?.add(to: .main, forMode: .common)
        }
        let maximum = Float(screen.maximumFramesPerSecond)
        displayLink?.preferredFrameRateRange = CAFrameRateRange(
            minimum: 1, maximum: maximum, preferred: maximum
        )
        displayLink?.isPaused = framesSuspended
    }

    func wakeFrames() {
        guard !framesSuspended, hostView.window != nil else { return }
        displayLink?.isPaused = false
    }

    func firstFrameRendered() {
        // Let the submitted Metal drawable reach the compositor before uncovering it.
        DispatchQueue.main.async { [weak self] in
            self?.launchCover?.removeFromSuperview()
            self?.launchCover = nil
        }
    }

    func resumeFrames() {
        framesSuspended = false
        updateScreen()
        wakeFrames()
    }

    func pauseFrames() {
        framesSuspended = true
        displayLink?.isPaused = true
    }

    func setAppBackgroundDark(_ dark: Bool) {
        appBackgroundDark = dark
        view.backgroundColor = UIColor(named: "LaunchBackground")?.resolvedColor(
            with: UITraitCollection(userInterfaceStyle: dark ? .dark : .light)
        )
        view.window?.backgroundColor = view.backgroundColor
        setNeedsStatusBarAppearanceUpdate()
    }

    @objc private func renderFrame() {
        displayLink?.isPaused = gpui_ios_request_frame() == 0
    }

    deinit {
        displayLink?.invalidate()
    }
}

final class GPUIHostView: UIView {
    private var attached = false
    private var nextTouchIdentifier: UInt64 = 1
    private var touchIdentifiers: [ObjectIdentifier: UInt64] = [:]
    private var keyboardFrameInScreen: CGRect?
    let keyboardProxy = GPUITextInput(frame: .zero)
    // A control's configuration is fixed at creation, so a new style means a
    // new control.
    private var pasteControlStyle = PasteControlStyle.unset
    private lazy var pasteControl = makePasteControl(style: pasteControlStyle)
    // Once it recognizes, UIKit cancels the edge touch in this view, so GPUI
    // sees no pan to fling and no tap; an edge contact that never moves fails
    // it and reaches GPUI unchanged.
    private lazy var backSwipe: UIScreenEdgePanGestureRecognizer = {
        let recognizer = UIScreenEdgePanGestureRecognizer(
            target: self,
            action: #selector(backSwipeChanged(_:))
        )
        recognizer.edges = .left
        return recognizer
    }()

    override class var layerClass: AnyClass {
        CAMetalLayer.self
    }

    private var metalLayer: CAMetalLayer {
        layer as! CAMetalLayer
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        isOpaque = true
        backgroundColor = UIColor(named: "LaunchBackground", in: nil, compatibleWith: traitCollection)
        isMultipleTouchEnabled = true
        contentScaleFactor = traitCollection.displayScale

        metalLayer.pixelFormat = .bgra8Unorm
        metalLayer.framebufferOnly = true
        metalLayer.contentsScale = contentScaleFactor
        metalLayer.presentsWithTransaction = false

        keyboardProxy.keyEventTarget = self
        keyboardProxy.backgroundColor = .clear
        keyboardProxy.tintColor = .clear
        keyboardProxy.isAccessibilityElement = false
        keyboardProxy.autocorrectionType = .no
        keyboardProxy.autocapitalizationType = .none
        keyboardProxy.spellCheckingType = .no
        keyboardProxy.smartDashesType = .no
        keyboardProxy.smartQuotesType = .no
        keyboardProxy.smartInsertDeleteType = .no
        keyboardProxy.pasteConfiguration = UIPasteConfiguration(forAccepting: NSString.self)
        addSubview(keyboardProxy)
        addSubview(pasteControl)
        addGestureRecognizer(backSwipe)

        NotificationCenter.default.addObserver(
            self,
            selector: #selector(keyboardFrameChanged(_:)),
            name: UIResponder.keyboardWillChangeFrameNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(keyboardFrameChanged(_:)),
            name: UIResponder.keyboardWillHideNotification,
            object: nil
        )
        registerForTraitChanges([UITraitUserInterfaceStyle.self]) {
            (view: GPUIHostView, _: UITraitCollection) in
            guard view.attached else { return }
            gpui_ios_appearance_changed(view.traitCollection.userInterfaceStyle == .dark ? 1 : 0)
        }
        GPUIHostBridge.view = self
    }

    required init?(coder: NSCoder) {
        fatalError("GPUIHostView must be created programmatically")
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
        if attached {
            gpui_ios_detach_view(Unmanaged.passUnretained(self).toOpaque())
        }
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        GPUIHostBridge.controller?.updateScreen()
        guard let screen = window?.windowScene?.screen else { return }
        contentScaleFactor = screen.scale
        metalLayer.contentsScale = screen.scale
        if attached {
            gpui_ios_scale_factor_changed(Float(screen.scale))
        }
    }

    @discardableResult
    func attachToGPUI() -> Bool {
        if attached { return true }
        guard window != nil, bounds.width > 0, bounds.height > 0 else { return false }

        let result = gpui_ios_attach_view(
            Unmanaged.passUnretained(self).toOpaque(),
            Float(bounds.width),
            Float(bounds.height),
            Float(contentScaleFactor),
            traitCollection.userInterfaceStyle == .dark ? 1 : 0
        )
        attached = result != 0
        if attached {
            sendGeometry()
            sendSafeArea()
        }
        return attached
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        keyboardProxy.frame = bounds
        metalLayer.contentsScale = contentScaleFactor
        metalLayer.drawableSize = CGSize(
            width: bounds.width * contentScaleFactor,
            height: bounds.height * contentScaleFactor
        )
        if attached {
            sendSafeArea()
            sendKeyboardCover()
            sendGeometry()
        }
    }

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        if attached {
            sendSafeArea()
        }
    }

    private func sendGeometry() {
        gpui_ios_resize(
            Float(bounds.width),
            Float(bounds.height),
            Float(contentScaleFactor)
        )
    }

    private func sendSafeArea() {
        gpui_ios_safe_area_changed(
            Float(safeAreaInsets.top),
            Float(safeAreaInsets.right),
            Float(safeAreaInsets.bottom),
            Float(safeAreaInsets.left)
        )
    }

    @objc private func keyboardFrameChanged(_ notification: Notification) {
        guard let frame = notification.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect,
              notification.name != UIResponder.keyboardWillHideNotification
        else {
            keyboardFrameInScreen = nil
            sendKeyboardCover()
            return
        }

        keyboardFrameInScreen = frame
        sendKeyboardCover()
    }

    private func sendKeyboardCover() {
        let coveredHeight: CGFloat
        if let keyboardFrameInScreen {
            let localFrame = convert(keyboardFrameInScreen, from: nil)
            let covered = bounds.intersection(localFrame)
            coveredHeight = covered.isNull ? 0 : covered.height
        } else {
            coveredHeight = 0
        }
        gpui_ios_keyboard_frame_changed(Float(coveredHeight))
    }

    func showKeyboard() {
        keyboardProxy.becomeFirstResponder()
    }

    func hideKeyboard() {
        pasteControl.isHidden = true
        keyboardProxy.resignFirstResponder()
    }

    private var pendingPasteFrame = CGRect.zero
    private var pendingPasteStyle = PasteControlStyle.unset
    private var pasteUpdatePending = false

    /// The edit menu's Paste item is UIKit's own control, drawn in the
    /// menu's colours and corners so it reads as one of the menu's items.
    private func makePasteControl(style: PasteControlStyle) -> UIPasteControl {
        let configuration = UIPasteControl.Configuration()
        configuration.displayMode = .labelOnly
        if style != .unset {
            configuration.baseForegroundColor = style.foregroundColor
            configuration.baseBackgroundColor = style.backgroundColor
            configuration.cornerStyle = .fixed
            configuration.cornerRadius = style.cornerRadius
        }
        let control = UIPasteControl(configuration: configuration)
        control.target = keyboardProxy
        control.isHidden = true
        return control
    }

    func setPasteControl(frame: CGRect, style: PasteControlStyle) {
        pendingPasteFrame = frame
        if !frame.isEmpty {
            pendingPasteStyle = style
        }
        guard !pasteUpdatePending else { return }
        pasteUpdatePending = true
        // Render clears the old menu before paint places its replacement. Apply
        // both together so a redraw cannot cancel a control's tracked touch.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.pasteUpdatePending = false
            if self.pendingPasteStyle != self.pasteControlStyle {
                self.pasteControlStyle = self.pendingPasteStyle
                self.pasteControl.removeFromSuperview()
                self.pasteControl = self.makePasteControl(style: self.pasteControlStyle)
                self.addSubview(self.pasteControl)
            }
            self.pasteControl.frame = self.pendingPasteFrame
            self.pasteControl.isHidden = self.pendingPasteFrame.isEmpty || !self.keyboardProxy.isFirstResponder
        }
    }

    func configureTextInput(
        autocorrect: Bool,
        autocapitalize: UInt32,
        suggestions: Bool,
        inputAction: UInt32
    ) {
        keyboardProxy.autocorrectionType = autocorrect ? .yes : .no
        keyboardProxy.spellCheckingType = suggestions ? .yes : .no
        keyboardProxy.inlinePredictionType = suggestions ? .yes : .no
        keyboardProxy.smartDashesType = autocorrect ? .yes : .no
        keyboardProxy.smartQuotesType = autocorrect ? .yes : .no
        keyboardProxy.smartInsertDeleteType = autocorrect ? .yes : .no
        keyboardProxy.autocapitalizationType = switch autocapitalize {
        case 1: .words
        case 2: .sentences
        case 3: .allCharacters
        default: .none
        }
        keyboardProxy.returnKeyType = switch inputAction {
        case 1: .default
        case 2: .done
        case 3: .go
        case 4: .next
        case 5: .continue
        case 6: .search
        case 7: .send
        default: .default
        }
        if keyboardProxy.isFirstResponder {
            keyboardProxy.reloadInputViews()
        }
    }

    override func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard gestureRecognizer === backSwipe else {
            return super.gestureRecognizerShouldBegin(gestureRecognizer)
        }
        return tcode_ios_can_swipe_back() != 0
    }

    @objc private func backSwipeChanged(_ recognizer: UIScreenEdgePanGestureRecognizer) {
        let phase: UInt8
        switch recognizer.state {
        case .began: phase = 0
        case .changed: phase = 1
        case .ended: phase = 2
        case .cancelled, .failed: phase = 3
        default: return
        }
        tcode_ios_edge_back(phase, Float(recognizer.translation(in: self).x),
                            Float(recognizer.velocity(in: self).x))
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesBegan(touches, with: event)
        for touch in touches {
            touchIdentifiers[ObjectIdentifier(touch)] = nextTouchIdentifier
            nextTouchIdentifier &+= 1
        }
        sendTouches(touches, event: event, phase: .began)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesMoved(touches, with: event)
        sendTouches(touches, event: event, phase: .moved)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesEnded(touches, with: event)
        sendTouches(touches, event: event, phase: .ended)
        for touch in touches {
            touchIdentifiers.removeValue(forKey: ObjectIdentifier(touch))
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesCancelled(touches, with: event)
        sendTouches(touches, event: event, phase: .cancelled)
        for touch in touches {
            touchIdentifiers.removeValue(forKey: ObjectIdentifier(touch))
        }
    }

    private enum ForwardedTouchPhase {
        case began
        case moved
        case ended
        case cancelled
    }

    private func sendTouches(
        _ touches: Set<UITouch>,
        event: UIEvent?,
        phase: ForwardedTouchPhase
    ) {
        var serialized = touches.compactMap { touch -> GpuiIosTouch? in
            // Native subviews own their touches; replaying them in GPUI can
            // redraw and remove a control before UIKit delivers its action.
            guard touch.view === self else { return nil }
            guard let identifier = touchIdentifiers[ObjectIdentifier(touch)] else { return nil }
            let position = touch.location(in: self)
            let predicted = phase == .moved ? event?.predictedTouches(for: touch)?.last : nil
            let predictedPosition = predicted?.location(in: self) ?? position
            let maximumForce = touch.maximumPossibleForce
            let normalizedForce = maximumForce > 0 ? touch.force / maximumForce : 0
            return GpuiIosTouch(
                identifier: identifier,
                x: position.x,
                y: position.y,
                predicted_x: predictedPosition.x,
                predicted_y: predictedPosition.y,
                force: Float(normalizedForce),
                has_prediction: predicted == nil ? 0 : 1,
                has_force: maximumForce > 0 ? 1 : 0
            )
        }
        serialized.sort { $0.identifier < $1.identifier }
        serialized.withUnsafeBufferPointer { buffer in
            switch phase {
            case .began:
                gpui_ios_touches_began(buffer.baseAddress, buffer.count)
            case .moved:
                gpui_ios_touches_moved(buffer.baseAddress, buffer.count)
            case .ended:
                gpui_ios_touches_ended(buffer.baseAddress, buffer.count)
            case .cancelled:
                gpui_ios_touches_cancelled(buffer.baseAddress, buffer.count)
            }
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesBegan(presses, with: event)
        if !keyboardProxy.isFirstResponder { sendPresses(presses, down: true, repeatKey: false) }
    }

    override func pressesChanged(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesChanged(presses, with: event)
        if !keyboardProxy.isFirstResponder { sendPresses(presses, down: true, repeatKey: true) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesEnded(presses, with: event)
        if !keyboardProxy.isFirstResponder { sendPresses(presses, down: false, repeatKey: false) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesCancelled(presses, with: event)
        if !keyboardProxy.isFirstResponder { sendPresses(presses, down: false, repeatKey: false) }
    }

    @discardableResult
    func sendPresses(_ presses: Set<UIPress>, down: Bool, repeatKey: Bool) -> Set<UIPress> {
        Set(presses.filter { press in
            guard let key = press.key else { return true }
            let name = gpuiKeyName(for: key)
            return !sendKey(
                name: name,
                character: gpuiCharacter(for: key, name: name),
                modifiers: modifierBits(key.modifierFlags),
                down: down,
                repeatKey: repeatKey
            )
        })
    }
}

private func gpuiKeyName(for key: UIKey) -> String {
    switch key.charactersIgnoringModifiers {
    case UIKeyCommand.inputUpArrow: "up"
    case UIKeyCommand.inputDownArrow: "down"
    case UIKeyCommand.inputLeftArrow: "left"
    case UIKeyCommand.inputRightArrow: "right"
    case UIKeyCommand.inputEscape: "escape"
    case UIKeyCommand.inputDelete: "backspace"
    case "\r", "\n": "enter"
    case "\t": "tab"
    case " ": "space"
    default: key.charactersIgnoringModifiers.lowercased()
    }
}

private func gpuiCharacter(for key: UIKey, name: String) -> String {
    switch name {
    case "up", "down", "left", "right", "escape", "backspace", "tab": ""
    case "enter": "\n"
    case "space": " "
    default: key.characters
    }
}

private func modifierBits(_ flags: UIKeyModifierFlags) -> UInt32 {
    var bits: UInt32 = 0
    if flags.contains(.shift) { bits |= 1 }
    if flags.contains(.control) { bits |= 2 }
    if flags.contains(.alternate) { bits |= 4 }
    if flags.contains(.command) { bits |= 8 }
    return bits
}

@discardableResult
func sendKey(
    name: String,
    character: String,
    modifiers: UInt32,
    down: Bool,
    repeatKey: Bool
) -> Bool {
    var handled = false
    withUTF8(name) { keyBytes, keyLength in
        withUTF8(character) { characterBytes, characterLength in
            handled = gpui_ios_key_event(
                keyBytes,
                keyLength,
                characterBytes,
                characterLength,
                modifiers,
                down ? 1 : 0,
                repeatKey ? 1 : 0
            ) != 0
        }
    }
    return handled
}

/// How the edit menu draws its Paste item; colours are sRGB components.
struct PasteControlStyle: Equatable {
    static let unset = PasteControlStyle(foreground: [], background: [], cornerRadius: 0)

    var foreground: [Double]
    var background: [Double]
    var cornerRadius: Double

    var foregroundColor: UIColor { Self.color(foreground) }
    var backgroundColor: UIColor { Self.color(background) }

    private static func color(_ components: [Double]) -> UIColor {
        UIColor(
            red: components[0],
            green: components[1],
            blue: components[2],
            alpha: components[3]
        )
    }
}
