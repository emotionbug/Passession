import Cocoa
import FlutterMacOS
import desktop_multi_window

class MainFlutterWindow: NSWindow {
  override func awakeFromNib() {
    let flutterViewController = FlutterViewController()
    let windowFrame = self.frame
    self.contentViewController = flutterViewController
    self.setFrame(windowFrame, display: false)
    self.title = ""
    self.isRestorable = false

    // The plugin creates a separate engine for each secondary window.
    FlutterMultiWindowPlugin.setOnWindowCreatedCallback { controller in
      RegisterGeneratedPlugins(registry: controller)
    }
    (NSApp.delegate as? AppDelegate)?.attachDesktopHost(to: flutterViewController)

    RegisterGeneratedPlugins(registry: flutterViewController)

    super.awakeFromNib()
    // Dart restores geometry and shows the primary user window after host setup.
    self.orderOut(nil)
  }
}
