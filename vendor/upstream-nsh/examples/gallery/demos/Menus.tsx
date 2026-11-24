import { type JSX, Scroll, setColorScheme, signal, Text, token, View } from "nsh";
import { ContextMenu, Menu, MenuItem, MenuSeparator, type OpenChangeReason, Submenu } from "nsh/base";
import { platform } from "nsh";
import { keymap } from "nsh";
import { Icon, LucideIcon, type LucideIconName } from "../icons";
import { toast, updateToast } from "../toast";
import { alpha, Badge, Button, Caption, Card, colors, DemoPage, finish, isDark, mono, PopIn, type PopupSide, popupSurface, Row, Segmented, Tip } from "../ui";

// The look of every menu here is the gallery's: `nsh/base` draws none.

const pointer = platform.idiom === "desktop";

/** The sheet at the bottom on touch screens; on desktops the popup's box
 * keeps no look, its `Panel` has it. */
const sheet = pointer
  ? undefined
  : {
      backgroundColor: colors.surface,
      borderTopLeftRadius: 20,
      borderTopRightRadius: 20,
      paddingHorizontal: 10,
      paddingTop: 10,
      paddingBottom: 34,
    };

/**
 * A menu's items on their surface: on desktops the gallery's popup surface
 * coming in from the trigger's side (`PopIn`: a fade, 96 % to full size,
 * 4 points of slide, 150 ms), on touch screens the items alone, in the
 * sheet that slides up. `flush` drops the padding (a menu that scrolls).
 */
function Panel(props: { side?: PopupSide; flush?: boolean; children: JSX.Element }) {
  if (!pointer) return props.children;
  return (
    <PopIn side={props.side} style={{ ...popupSurface(), minWidth: 200, padding: props.flush ? 0 : 4 }}>
      {props.children}
    </PopIn>
  );
}

const backdrop = { backgroundColor: "rgba(0,0,0,0.45)" };

/** An item under the pointer or the keyboard, and pressed: a flat neutral
 * fill, the theme's `accent`, a shade deeper when pressed. */
const hovered = () => ({ backgroundColor: token("accent") });
const pressed = () => ({ backgroundColor: alpha(colors.text, 0.1) });

/** An item's height, corners and type. */
const item = pointer ? { height: 26, radius: 6, font: 13, note: 11.5, icon: 14 } : { height: 46, radius: 10, font: 16, note: 14, icon: 18 };

/** One row of a menu: an icon, the label, a note (a shortcut, a value), a mark (a check, a dot). */
function Item(props: {
  label: string;
  icon?: LucideIconName;
  onSelect?: () => void;
  shortcut?: string;
  note?: string;
  mark?: "check" | "dot" | null;
  disabled?: boolean;
  danger?: boolean;
  keepOpen?: boolean;
}) {
  return (
    <MenuItem
      onSelect={props.onSelect}
      disabled={props.disabled}
      closeOnSelect={!props.keepOpen}
      hoverStyle={hovered()}
      activeStyle={pressed()}
      disabledStyle={{ opacity: 0.35 }}
      style={{ flexDirection: "row", alignItems: "center", gap: 8, height: item.height, paddingHorizontal: 8, borderRadius: item.radius }}
    >
      <If when={props.icon}>
        <LucideIcon name={props.icon!} size={item.icon} tint={props.danger ? colors.danger : colors.muted} />
      </If>
      <Text style={{ flex: 1, fontSize: item.font, color: props.danger ? colors.danger : colors.text }}>{props.label}</Text>
      <If when={props.shortcut ?? props.note}>
        <Text style={{ fontSize: item.note, color: colors.faint }}>{props.shortcut ?? props.note}</Text>
      </If>
      {/* The check or the dot at the trailing edge. */}
      <If when={props.mark !== undefined}>
        <View style={{ width: 16, alignItems: "center" }}>
          <If when={props.mark === "check"}>
            <Icon name="check" size={14} tint={colors.accent} />
          </If>
          <If when={props.mark === "dot"}>
            <View style={{ width: 7, height: 7, borderRadius: 4, backgroundColor: colors.accent }} />
          </If>
        </View>
      </If>
    </MenuItem>
  );
}

/** The row that opens a submenu, and the row a sheet shows to come back. */
function SubmenuOf(props: { label: string; icon?: LucideIconName; children: JSX.Element; disabled?: boolean }) {
  return (
    <Submenu
      disabled={props.disabled}
      trigger={
        <View
          hoverStyle={hovered()}
          activeStyle={pressed()}
          style={{ flexDirection: "row", alignItems: "center", gap: 8, height: item.height, paddingHorizontal: 8, borderRadius: item.radius, opacity: props.disabled ? 0.35 : 1 }}
        >
          <If when={props.icon}>
            <LucideIcon name={props.icon!} size={item.icon} tint={colors.muted} />
          </If>
          <Text style={{ flex: 1, fontSize: item.font, color: colors.text }}>{props.label}</Text>
          <Icon name="chevron" size={pointer ? 12 : 14} tint={colors.faint} />
        </View>
      }
      back={
        <View activeStyle={pressed()} style={{ flexDirection: "row", alignItems: "center", gap: 6, height: item.height, paddingHorizontal: 8, borderRadius: item.radius }}>
          <Icon name="back" size={14} tint={colors.accent} />
          <Text style={{ fontSize: item.font, fontWeight: "600", color: colors.accent }}>{props.label}</Text>
        </View>
      }
    >
      <Panel side="right">{props.children}</Panel>
    </Submenu>
  );
}

/** A group's title: not an item, nothing to press. */
function Label(props: { text: string }) {
  return (
    <Text style={{ fontSize: pointer ? 11 : 13, fontWeight: "600", color: colors.faint, paddingHorizontal: 8, paddingTop: 6, paddingBottom: 2 }}>
      {props.text}
    </Text>
  );
}

function Separator() {
  // A groove: a hairline with a lip of light under it.
  return (
    <MenuSeparator
      style={{ height: 1, marginTop: 4, marginBottom: 5, marginHorizontal: 4, backgroundColor: colors.line, boxShadow: `0 1px 0 ${isDark() ? "rgba(255,255,255,0.05)" : "#ffffff"}` }}
    />
  );
}

/** What opens a menu: a quiet menu bar title (`bar`), else a raised button. */
function Chip(props: { label: string; icon?: "plus" | "search" | "star"; bar?: boolean; open?: boolean }) {
  // Open: selected, as the shared `Button`'s `selected` (a menu bar title
  // keeps the hover's accent fill, a raised button its pressed look).
  const look = () =>
    props.bar
      ? { borderColor: "transparent", backgroundColor: props.open ? token("accent") : "transparent" }
      : { backgroundColor: colors.raised, ...(props.open ? finish.pressed : finish.raised) };
  return (
    <View
      hoverStyle={props.open ? undefined : props.bar ? { backgroundColor: alpha(colors.text, 0.06) } : finish.hover}
      activeStyle={props.bar ? { backgroundColor: alpha(colors.text, 0.1) } : finish.pressed}
      style={{
        flexDirection: "row",
        alignItems: "center",
        justifyContent: "center",
        gap: 6,
        alignSelf: "flex-start",
        height: pointer ? 28 : 36,
        paddingHorizontal: props.bar ? 10 : pointer ? 10 : 14,
        borderRadius: pointer ? 7 : 10,
        borderWidth: 1,
        ...look(),
        transitionProperty: "background-color, border-color, box-shadow",
        transitionDuration: 120,
      }}
    >
      <If when={props.icon}>
        <Icon name={props.icon!} size={pointer ? 13 : 14} tint={colors.muted} />
      </If>
      <Text style={{ fontSize: pointer ? 13 : 15, fontWeight: "500", color: colors.text }}>{props.label}</Text>
    </View>
  );
}

const ZONES = [
  "UTC−10 Honolulu", "UTC−8 Los Angeles", "UTC−7 Denver", "UTC−6 Chicago", "UTC−5 New York", "UTC−3 São Paulo",
  "UTC±0 London", "UTC+1 Berlin", "UTC+2 Cairo", "UTC+3 Moscow", "UTC+4 Dubai", "UTC+5:30 Mumbai", "UTC+7 Bangkok",
  "UTC+8 Hong Kong", "UTC+8 Shanghai", "UTC+8 Singapore", "UTC+9 Tokyo", "UTC+9 Seoul", "UTC+10 Sydney", "UTC+12 Auckland",
];

type File = { id: number; name: string; starred: boolean };

/**
 * Menus of `nsh/base`, as complete as an app's: a menu bar whose menus have
 * shortcuts, submenus three levels deep, checkable items and a choice of one,
 * group titles, disabled and destructive items; menus on the rows of a list
 * (the secondary button or a held finger, and a ⋯ button); where a popup
 * goes; a long menu that scrolls; a menu a shortcut opens. On desktops they
 * are popups with cascading submenus that grow out of their trigger, on
 * touch screens sheets that slide up, whose submenus take the place of
 * their items.
 */
export function Menus() {
  const events = signal<string[]>([]);
  const log = (what: string) => events.set([what, ...events()].slice(0, 6));
  // The menu shown: its trigger shows as selected while it is open.
  const shown = signal<string | null>(null);
  const isOpen = (menu: string) => shown() === menu;
  const changed = (menu: string) => ({ open, reason }: { open: boolean; reason: OpenChangeReason }) => {
    if (open) shown.set(menu);
    else if (shown() === menu) shown.set(null);
    log(`${menu} ${open ? "opened" : "closed"} · ${reason}`);
  };

  function MenuBar() {
    const history = signal<string[]>([]);
    const undone = signal<string[]>([]);
    const sidebar = signal(true);
    const statusBar = signal(false);
    const zoom = signal<"Small" | "Medium" | "Large">("Medium");
    const recent = signal(["Budget.numbers", "Roadmap.md", "Trip photos"]);
    const act = (what: string) => {
      history.set([...history(), what]);
      undone.set([]);
      log(`chose ${what}`);
    };
    const undo = () => {
      const last = history().at(-1);
      if (!last) return;
      history.set(history().slice(0, -1));
      undone.set([...undone(), last]);
      log(`undid ${last}`);
    };
    const redo = () => {
      const last = undone().at(-1);
      if (!last) return;
      undone.set(undone().slice(0, -1));
      history.set([...history(), last]);
      log(`redid ${last}`);
    };
    // The shortcuts the items show work without the menu too.
    keymap.bind({
      "mod+alt+n": () => act("New document"),
      "mod+alt+s": () => act("Save"),
      "mod+alt+z": undo,
      "mod+alt+shift+z": redo,
    });
    const key = (k: string) => (platform.idiom === "desktop" ? keymap.label(k) : undefined);
    return (
      <Card icon="lists" title="Menu bar" subtitle={platform.idiom === "desktop" ? "Hover across submenus; the shortcuts work too" : "Each menu a sheet; submenus take its place"}>
        <View style={{ flexDirection: "row", gap: 2, marginLeft: pointer ? -10 : 0 }}>
          <Menu offset={4} margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed("File")} trigger={<Chip bar label="File" open={isOpen("File")} />}>
            <Panel>
              <Item label="New document" icon="file-plus" shortcut={key("mod+alt+n")} onSelect={() => act("New document")} />
              <Item label="New window" icon="panel-top" onSelect={() => act("New window")} />
              <SubmenuOf label="Open recent" icon="clock">
                <Each of={recent()}>{(name) => <Item label={name()} onSelect={() => act(`Open ${name()}`)} />}</Each>
                <Separator />
                <Item label="Clear recent" disabled={recent().length === 0} onSelect={() => recent.set([])} />
              </SubmenuOf>
              <Separator />
              <Item label="Save" icon="download" shortcut={key("mod+alt+s")} onSelect={() => act("Save")} />
              <SubmenuOf label="Export as" icon="share-2">
                <Item label="PDF" onSelect={() => act("Export PDF")} />
                <SubmenuOf label="Image">
                  <Item label="PNG" onSelect={() => act("Export PNG")} />
                  <Item label="JPEG" onSelect={() => act("Export JPEG")} />
                  <Item label="WebP" onSelect={() => act("Export WebP")} />
                </SubmenuOf>
                <SubmenuOf label="Web page" disabled>
                  <Item label="HTML" />
                </SubmenuOf>
              </SubmenuOf>
              <Separator />
              <Item label="Delete document" icon="trash" danger onSelect={() => act("Delete document")} />
            </Panel>
          </Menu>
          <Menu offset={4} margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed("Edit")} trigger={<Chip bar label="Edit" open={isOpen("Edit")} />}>
            <Panel>
              <Item label={history().length ? `Undo ${history().at(-1)}` : "Undo"} icon="undo-2" shortcut={key("mod+alt+z")} disabled={history().length === 0} onSelect={undo} />
              <Item label={undone().length ? `Redo ${undone().at(-1)}` : "Redo"} icon="redo-2" shortcut={key("mod+alt+shift+z")} disabled={undone().length === 0} onSelect={redo} />
              <Separator />
              <Label text="Clipboard" />
              <Item label="Cut" icon="scissors" onSelect={() => act("Cut")} />
              <Item label="Copy" icon="copy" onSelect={() => act("Copy")} />
              <Item label="Paste" icon="clipboard-paste" onSelect={() => act("Paste")} />
            </Panel>
          </Menu>
          <Menu offset={4} margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed("View")} trigger={<Chip bar label="View" open={isOpen("View")} />}>
            <Panel>
              <Label text="Show" />
              <Item label="Sidebar" mark={sidebar() ? "check" : null} keepOpen onSelect={() => sidebar.set(!sidebar())} />
              <Item label="Status bar" mark={statusBar() ? "check" : null} keepOpen onSelect={() => statusBar.set(!statusBar())} />
              <Separator />
              <Label text="Zoom" />
              <Each of={["Small", "Medium", "Large"] as const}>
                {(size) => <Item label={size()} mark={zoom() === size() ? "dot" : null} keepOpen onSelect={() => zoom.set(size())} />}
              </Each>
            </Panel>
          </Menu>
        </View>
        <Caption label={`Sidebar ${sidebar() ? "on" : "off"} · status bar ${statusBar() ? "on" : "off"} · zoom ${zoom()}`} />
        <Caption label={history().length ? `Done: ${history().join(", ")}` : "Nothing done yet"} />
      </Card>
    );
  }

  function Rows() {
    let next = 5;
    const files = signal<File[]>([
      { id: 1, name: "Invoice March.pdf", starred: false },
      { id: 2, name: "Holiday.jpg", starred: true },
      { id: 3, name: "Notes.txt", starred: false },
      { id: 4, name: "Slides.key", starred: false },
    ]);
    const update = (id: number, change: (f: File) => File) => files.set(files().map((f) => (f.id === id ? change(f) : f)));
    const actions = (file: () => File) => (
      <Panel>
        <Item label="Open" icon="external-link" onSelect={() => log(`open ${file().name}`)} />
        <Item label={file().starred ? "Unstar" : "Star"} icon="star" onSelect={() => update(file().id, (f) => ({ ...f, starred: !f.starred }))} />
        <Item
          label="Duplicate"
          icon="copy"
          onSelect={() => {
            const f = file();
            const at = files().findIndex((x) => x.id === f.id);
            const copy = { id: next++, name: f.name.replace(/(\.[^.]+)?$/, " copy$1"), starred: false };
            files.set([...files().slice(0, at + 1), copy, ...files().slice(at + 1)]);
          }}
        />
        <SubmenuOf label="Move to" icon="folder-input">
          <Item label="Documents" onSelect={() => log(`moved ${file().name} to Documents`)} />
          <Item label="Archive" onSelect={() => log(`moved ${file().name} to Archive`)} />
        </SubmenuOf>
        <Separator />
        <Item label="Delete" icon="trash" danger onSelect={() => files.set(files().filter((f) => f.id !== file().id))} />
      </Panel>
    );
    return (
      <Card icon="photos" title="On the rows of a list" subtitle={platform.idiom === "desktop" ? "Right click a row, or its ⋯ button" : "Hold a finger on a row, or press its ⋯"}>
        <If when={files().length > 0} else={<Caption label="No files left" />}>
          <View>
            <Each of={files()} key={(f) => f.id}>
              {(file, i) => (
                <ContextMenu margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed(file().name)} trigger={
                  <Row first={i() === 0}>
                    <Icon name={file().starred ? "star" : "text"} size={16} tint={file().starred ? colors.accent : colors.muted} />
                    <Text style={{ numberOfLines: 1, flex: 1, fontSize: 14, color: colors.text }}>{file().name}</Text>
                    <Menu align="end" offset={4} margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed(`${file().name} ⋯`)} trigger={
                      <View
                        accessibilityLabel={`Actions for ${file().name}`}
                        tooltip={<Tip label="More actions" />}
                        hitSlop={8}
                        hoverStyle={hovered()}
                        activeStyle={{ backgroundColor: alpha(colors.text, 0.1) }}
                        style={{
                          paddingHorizontal: 8,
                          height: 28,
                          justifyContent: "center",
                          borderRadius: 6,
                          backgroundColor: isOpen(`${file().name} ⋯`) ? token("accent") : "transparent",
                        }}
                      >
                        <LucideIcon name="ellipsis" size={16} tint={colors.muted} />
                      </View>
                    }>
                      {actions(file)}
                    </Menu>
                  </Row>
                }>
                  {actions(file)}
                </ContextMenu>
              )}
            </Each>
          </View>
        </If>
      </Card>
    );
  }

  function Placement() {
    const placement = signal<"top" | "bottom" | "left" | "right">("bottom");
    const align = signal<"start" | "center" | "end">("start");
    return (
      <Card icon="components" title="Where it goes" subtitle={platform.idiom === "desktop" ? "The side and alignment; it flips where there is no room" : "Phones show menus as sheets: try a desktop"}>
        <Segmented
          size="small"
          options={[{ value: "top", label: "Top" }, { value: "bottom", label: "Bottom" }, { value: "left", label: "Left" }, { value: "right", label: "Right" }]}
          value={placement()}
          onChange={(v) => placement.set(v)}
        />
        <Segmented
          size="small"
          options={[{ value: "start", label: "Start" }, { value: "center", label: "Center" }, { value: "end", label: "End" }]}
          value={align()}
          onChange={(v) => align.set(v)}
        />
        <View style={{ alignItems: "center", paddingVertical: 24 }}>
          <Menu placement={placement()} align={align()} offset={6} margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed("Placed")} trigger={<Chip label={`${placement()} · ${align()}`} open={isOpen("Placed")} />}>
            <Panel side={placement()}>
              <Item label="One" onSelect={() => log("chose One")} />
              <Item label="Two" onSelect={() => log("chose Two")} />
              <Item label="Three" onSelect={() => log("chose Three")} />
            </Panel>
          </Menu>
        </View>
      </Card>
    );
  }

  function LongList() {
    const zone = signal("UTC+8 Hong Kong");
    return (
      <Card icon="scroll" title="A long menu" subtitle="Twenty choices in a menu that scrolls">
        <Menu offset={4} margin={8} popupStyle={sheet} backdropStyle={backdrop} onOpenChange={changed("Time zone")} trigger={<Chip label={zone()} icon="search" open={isOpen("Time zone")} />}>
          <Panel flush>
            <Scroll style={{ maxHeight: platform.idiom === "desktop" ? 280 : 360 }} contentStyle={{ padding: 6 }}>
              <Each of={ZONES}>{(z) => <Item label={z()} mark={zone() === z() ? "check" : null} onSelect={() => zone.set(z())} />}</Each>
            </Scroll>
          </Panel>
        </Menu>
      </Card>
    );
  }

  function Commands() {
    const open = signal(false);
    // A command that takes a while: its toast spins, then says how it went.
    const sync = () => {
      log("ran Sync now");
      void toast({ id: "sync", kind: "loading", title: "Syncing…", duration: Infinity });
      setTimeout(() => updateToast("sync", { kind: "success", title: "Up to date", description: "4 files synced", duration: 3000 }), 1200);
    };
    keymap.bind("mod+shift+p", () => open.set(!open()));
    return (
      <Card icon="spark" title="Opened by the app" subtitle="A controlled menu: a button or a shortcut opens it" right={platform.idiom === "desktop" ? <Badge label={keymap.label("mod+shift+p")} /> : undefined}>
        <Menu
          open={open()}
          onOpenChange={({ open: next, reason }) => {
            open.set(next);
            log(`commands ${next ? "opened" : "closed"} · ${reason}`);
          }}
          offset={4}
          margin={8}
          popupStyle={sheet}
          backdropStyle={backdrop}
          trigger={<Chip label="Commands" icon="plus" open={open()} />}
        >
          <Panel>
            <Label text="Commands" />
            <Item label="New note" icon="file-plus" onSelect={() => log("ran New note")} />
            <Item
              label={isDark() ? "Light appearance" : "Dark appearance"}
              icon={isDark() ? "sun" : "moon"}
              onSelect={() => {
                setColorScheme(isDark() ? "light" : "dark");
                log("ran Toggle theme");
              }}
            />
            <Item label="Sync now" icon="refresh-cw" onSelect={sync} />
          </Panel>
        </Menu>
        <Button label="Open the commands" variant="secondary" onPress={() => open.set(true)} />
      </Card>
    );
  }

  function Events() {
    return (
      <Card icon="system" title="Events" subtitle="onOpenChange with its reason, and what was chosen">
        <If when={events().length > 0} else={<Caption label="Open a menu" />}>
          <View style={{ gap: 4 }}>
            <Each of={events()}>
              {(e, i) => <Text style={{ fontSize: 12, fontFamily: mono, color: i() === 0 ? colors.text : colors.faint }}>{e()}</Text>}
            </Each>
          </View>
        </If>
      </Card>
    );
  }

  return <DemoPage cards={[MenuBar, Rows, Placement, LongList, Commands, Events]} />;
}
