package main

import (
	"archive/zip"
	"encoding/json"
	"errors"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"testing"
	"time"
)

// These fixtures execute copied real test executables. Identity is read from
// each image's own packaged metadata, not echoed from transaction expectations.
func init() {
	if os.Getenv("PASEO_COMPONENT_FIXTURE") != "1" {
		return
	}
	role := os.Getenv("PASEO_UPDATER_FIXTURE_ROLE")
	if !strings.HasPrefix(role, "component-") {
		return
	}
	exe, err := os.Executable()
	if err != nil {
		os.Exit(80)
	}
	if len(os.Args) == 2 && os.Args[1] == "--identity" {
		var v componentIdentity
		if _, err = readJSON(filepath.Join(filepath.Dir(exe), "identity.json"), &v); err != nil {
			os.Exit(81)
		}
		_ = json.NewEncoder(os.Stdout).Encode(struct {
			SchemaVersion int               `json:"schemaVersion"`
			ID            string            `json:"id"`
			Identity      componentIdentity `json:"identity"`
		}{1, "update-helper", v})
		os.Exit(0)
	}
	if role == "component-idle" || role == "component-daemon" {
		time.Sleep(25 * time.Second)
		os.Exit(0)
	}
	if role != "component-gui" {
		os.Exit(82)
	}
	directory := os.Getenv("PASEO_UPDATE_OPERATION_DIRECTORY")
	var r componentRequest
	if _, err = readJSON(filepath.Join(directory, "request.json"), &r); err != nil {
		os.Exit(83)
	}
	me, err := inspectProcess(os.Getpid())
	if err != nil {
		os.Exit(84)
	}
	var daemon *exec.Cmd
	var binding runtimeBinding
	lastPhase := ""
	defer func() {
		if daemon != nil {
			_ = daemon.Process.Kill()
			_ = daemon.Wait()
		}
	}()
	deadline := time.Now().Add(20 * time.Second)
	for time.Now().Before(deadline) {
		var status componentStatus
		if _, err = readJSON(filepath.Join(directory, "status.json"), &status); err != nil {
			time.Sleep(10 * time.Millisecond)
			continue
		}
		if (status.Phase == "checking" || status.Phase == "rollingBack") && status.Phase != lastPhase {
			if daemon != nil {
				_ = daemon.Process.Kill()
				_ = daemon.Wait()
				daemon = nil
			}
			set := r.TargetComponents
			name := "health.json"
			if status.Phase == "rollingBack" {
				set = r.Current
				name = "rollback-health.json"
			}
			actual := make(map[string]componentIdentity)
			for id, c := range set {
				var v componentIdentity
				if _, err = readJSON(filepath.Join(componentRoot(r, c), "identity.json"), &v); err != nil {
					os.Exit(87)
				}
				actual[id] = v
			}
			if _, err = os.Stat(filepath.Join(componentRoot(r, set["app"]), independentDaemonMarker)); err == nil && status.Phase == "checking" {
				delete(actual, "daemon")
				if os.Getenv("PASEO_COMPONENT_HEALTH") == "wrong" {
					delete(actual, "app")
				}
				if err = atomicJSON(filepath.Join(directory, name), map[string]any{
					"schemaVersion": 2, "operationId": r.OperationID, "nonce": status.Nonce,
					"components": actual, "guiProcess": me.identity,
				}); err != nil {
					os.Exit(88)
				}
				lastPhase = status.Phase
				continue
			}
			image := componentImage(r, set["daemon"])
			daemon = exec.Command(image)
			daemon.Env = append(os.Environ(), "PASEO_UPDATER_FIXTURE_ROLE=component-daemon")
			if err = daemon.Start(); err != nil {
				os.Exit(85)
			}
			dp, e := inspectProcess(daemon.Process.Pid)
			if e != nil {
				os.Exit(86)
			}
			binding = runtimeBinding{dp.identity, image}
			server := r.ServerID
			if (os.Getenv("PASEO_COMPONENT_HEALTH") == "wrong" || os.Getenv("PASEO_COMPONENT_HEALTH") == "refuse") && status.Phase == "checking" {
				server = "wrong-server"
			}
			h := componentHealth{2, r.OperationID, status.Nonce, server, r.DaemonHome, actual, me.identity, dp.identity, image, []runtimeBinding{binding}}
			if err = atomicJSON(filepath.Join(directory, name), h); err != nil {
				os.Exit(88)
			}
			lastPhase = status.Phase
		}
		if _, err = os.Lstat(filepath.Join(directory, "stop.json")); err == nil {
			if os.Getenv("PASEO_COMPONENT_HEALTH") == "refuse" {
				time.Sleep(10 * time.Millisecond)
				continue
			}
			bindings := []runtimeBinding{}
			if daemon != nil {
				_ = daemon.Process.Kill()
				_ = daemon.Wait()
				daemon = nil
				bindings = append(bindings, binding)
			}
			if err = atomicJSON(filepath.Join(directory, "stopped.json"), map[string]any{"schemaVersion": 2, "operationId": r.OperationID, "nonce": status.Nonce, "runtimeProcesses": bindings}); err != nil {
				os.Exit(89)
			}
			if r.RestartGroup == "gui" {
				os.Exit(0)
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	os.Exit(0)
}

func componentFixtureIdentity(id, version string) componentIdentity {
	abi := uint32(1)
	if id == "update-helper" {
		abi = 2
	}
	ch := "a"
	if version == "2.0.0" {
		ch = "b"
	}
	return componentIdentity{version, strings.Repeat(ch, 40), strings.Repeat(ch, 64), abi}
}
func zipComponentFixture(t *testing.T, root, destination string) {
	t.Helper()
	f, err := os.OpenFile(destination, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		t.Fatal(err)
	}
	z := zip.NewWriter(f)
	err = filepath.WalkDir(root, func(p string, d os.DirEntry, e error) error {
		if e != nil || p == root {
			return e
		}
		rel, e := filepath.Rel(root, p)
		if e != nil {
			return e
		}
		st, e := d.Info()
		if e != nil {
			return e
		}
		h, e := zip.FileInfoHeader(st)
		if e != nil {
			return e
		}
		h.Name = filepath.ToSlash(rel)
		h.Method = zip.Deflate
		if d.IsDir() {
			h.Name += "/"
			_, e = z.CreateHeader(h)
			return e
		}
		w, e := z.CreateHeader(h)
		if e != nil {
			return e
		}
		in, e := os.Open(p)
		if e != nil {
			return e
		}
		_, e = io.Copy(w, in)
		ce := in.Close()
		if e == nil {
			e = ce
		}
		return e
	})
	if err != nil {
		t.Fatal(err)
	}
	if err = z.Close(); err != nil {
		t.Fatal(err)
	}
	if err = f.Close(); err != nil {
		t.Fatal(err)
	}
}
func componentFixture(t *testing.T, changed ...string) *componentOperation {
	t.Helper()
	if runtime.GOOS != "linux" {
		t.Skip("real component process fixtures exercise Linux; signed macOS and Windows ACL execution require native runners")
	}
	t.Setenv("PASEO_COMPONENT_FIXTURE", "1")
	t.Setenv("PASEO_UPDATER_FIXTURE_ROLE", "component-gui")
	base := canonicalTemp(t)
	root, store, home, opdir := filepath.Join(base, "installed"), filepath.Join(base, "store"), filepath.Join(base, "home"), filepath.Join(base, "operation")
	for _, p := range []string{root, store, home, opdir} {
		mustMkdir(t, p)
	}
	mustWrite(t, filepath.Join(home, "user-data"), []byte("preserve me"), 0600)
	target := "linux-x64"
	if runtime.GOARCH == "arm64" {
		target = "linux-arm64"
	}
	r := componentRequest{SchemaVersion: 2, Mode: "componentTransaction", OperationID: "11111111-1111-4111-8111-111111111111", OperationDirectory: opdir, StoreDirectory: store, BaseRoot: root, InstallationID: installationKey(root), Target: target, InventoryPath: filepath.Join(opdir, "inventory.json"), Current: componentSet{}, TargetComponents: componentSet{}, Staged: map[string]string{}, RestartGroup: "none", ServerID: "fixture-server", DaemonHome: home}
	bootstrap := bootstrapComponents{SchemaVersion: 1, Target: target, Components: map[string]componentRef{}, Roots: map[string]string{}, Packages: []componentPackage{}}
	inventory := componentInventory{SchemaVersion: 1, Version: "2.0.0", SourceSHA: strings.Repeat("b", 40), Channel: "stable", Targets: map[string]componentTarget{}}
	available := componentTarget{Components: map[string]componentRef{}, Packages: []componentPackage{}, AtomicGroups: [][]string{}}
	changes := make(map[string]bool)
	for _, id := range changed {
		changes[id] = true
	}
	for index, id := range componentIDs {
		p := filepath.Join(root, id)
		mustMkdir(t, p)
		image := "image" + exeSuffix()
		fixtureBinary(t, filepath.Join(p, image))
		v := componentFixtureIdentity(id, "1.0.0")
		if err := atomicJSON(filepath.Join(p, "identity.json"), v); err != nil {
			t.Fatal(err)
		}
		exeHash, err := componentFileHash(filepath.Join(p, image))
		if err != nil {
			t.Fatal(err)
		}
		c := componentRef{ID: id, Target: target, Identity: v, Artifact: componentArtifact{URL: "https://github.com/paseo/fixtures/releases/download/v1.0.0/" + id + ".zip", SHA256: strings.Repeat(string(rune('c'+index)), 64), Size: 1, Format: "zip", OriginTag: "v1.0.0"}, Executable: image, ExecutableSHA256: exeHash, Requires: map[string]uint32{}}
		writeComponentFixtureDescriptor(t, p, c)
		r.Current[id] = installedComponent{c, "base/" + id}
		r.TargetComponents[id] = r.Current[id]
		bootstrap.Components[id] = c
		bootstrap.Roots[id] = id
		if changes[id] {
			candidate := filepath.Join(base, "new-"+id)
			mustMkdir(t, candidate)
			fixtureBinary(t, filepath.Join(candidate, image))
			c.Identity = componentFixtureIdentity(id, "2.0.0")
			if err = atomicJSON(filepath.Join(candidate, "identity.json"), c.Identity); err != nil {
				t.Fatal(err)
			}
			if id == "app" && os.Getenv("PASEO_COMPONENT_MARK_APP") == "1" {
				mustWrite(t, filepath.Join(candidate, independentDaemonMarker), []byte(independentDaemonMarkerContents), 0600)
			}
			writeComponentFixtureDescriptor(t, candidate, c)
			archive := filepath.Join(opdir, id+".zip")
			zipComponentFixture(t, candidate, archive)
			archiveHash, e := componentFileHash(archive)
			if e != nil {
				t.Fatal(e)
			}
			st, e := os.Stat(archive)
			if e != nil {
				t.Fatal(e)
			}
			c.Artifact = componentArtifact{URL: "https://github.com/paseo/fixtures/releases/download/v2.0.0/" + id + ".zip", SHA256: archiveHash, Size: uint64(st.Size()), Format: "zip", OriginTag: "v2.0.0"}
			r.TargetComponents[id] = installedComponent{c, "objects/" + archiveHash}
			r.Staged[id] = archive
			if id == "app" || id == "tailnet" {
				r.RestartGroup = "gui"
			} else if id == "daemon" && r.RestartGroup != "gui" {
				r.RestartGroup = "daemon"
			}
		}
		available.Components[id] = c
	}
	inventory.Targets[target] = available
	if err := atomicJSON(filepath.Join(root, "components.json"), bootstrap); err != nil {
		t.Fatal(err)
	}
	if err := atomicJSON(r.InventoryPath, inventory); err != nil {
		t.Fatal(err)
	}
	var err error
	r.ManifestSHA512, err = digest(r.InventoryPath)
	if err != nil {
		t.Fatal(err)
	}
	r.CurrentExecutable = componentImage(r, r.Current["app"])
	parent, _ := fixtureProcess(t, r.CurrentExecutable, "component-idle")
	r.ParentProcess = parent
	dp, _ := fixtureProcess(t, componentImage(r, r.Current["daemon"]), "component-daemon")
	r.RuntimeProcesses = []runtimeBinding{{dp, componentImage(r, r.Current["daemon"])}}
	if err = atomicJSON(filepath.Join(opdir, "request.json"), r); err != nil {
		t.Fatal(err)
	}
	me, err := inspectProcess(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	o := &componentOperation{request: r, journal: componentJournal{SchemaVersion: 2, Step: "preparing", Status: componentStatus{SchemaVersion: 2, OperationID: r.OperationID, HelperProcess: me.identity, Nonce: strings.Repeat("a", 64), RestartGroup: r.RestartGroup, ExpectedComponents: componentIdentities(r.TargetComponents)}}, parentTimeout: time.Second, healthTimeout: 2 * time.Second, stopTimeout: 2 * time.Second, interval: 10 * time.Millisecond}
	t.Cleanup(func() {
		killFixture(o.journal.Launched)
		for _, name := range []string{"health.json", "rollback-health.json"} {
			var h componentHealth
			if _, e := readJSON(filepath.Join(opdir, name), &h); e == nil {
				killFixture(h.GUIProcess)
				for _, binding := range h.RuntimeProcesses {
					killFixture(binding.Process)
				}
			}
		}
		// Test-owned immutable payloads must become removable by t.TempDir.
		_ = filepath.WalkDir(store, func(p string, d os.DirEntry, e error) error {
			if e == nil && d.Type()&os.ModeSymlink == 0 {
				_ = os.Chmod(p, 0700)
			}
			return nil
		})
	})
	return o
}
func prepareComponentFixture(t *testing.T, o *componentOperation) {
	t.Helper()
	if err := validateComponentRequest(o.request, filepath.Join(o.request.OperationDirectory, "request.json")); err != nil {
		t.Fatal(err)
	}
	if err := o.phase("preparation", "fixture", false); err != nil {
		t.Fatal(err)
	}
	if err := o.prepare(); err != nil {
		t.Fatal(err)
	}
	if err := o.writeLeaseOwner(); err != nil {
		t.Fatal(err)
	}
}
func handoffComponentFixture(t *testing.T, o *componentOperation) {
	t.Helper()
	for _, binding := range o.request.RuntimeProcesses {
		killFixture(binding.Process)
	}
	if o.request.RestartGroup == "gui" {
		killFixture(o.request.ParentProcess)
	}
	if err := atomicJSON(filepath.Join(o.request.OperationDirectory, "handoff.json"), map[string]any{"schemaVersion": 2, "operationId": o.request.OperationID, "nonce": o.journal.Status.Nonce}); err != nil {
		t.Fatal(err)
	}
	if !o.awaitHandoff() {
		t.Fatal("real owned process handoff failed")
	}
}
func startRetainedComponentGUI(t *testing.T, o *componentOperation) {
	t.Helper()
	// Replace the idle fixture before preparation, making this real process the
	// exact long-lived owning GUI that survives daemon activation and rollback.
	killFixture(o.request.ParentProcess)
	cmd := exec.Command(o.request.CurrentExecutable)
	cmd.Env = append(os.Environ(), "PASEO_UPDATER_FIXTURE_ROLE=component-gui", "PASEO_UPDATE_OPERATION_DIRECTORY="+o.request.OperationDirectory)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	p, err := inspectProcess(cmd.Process.Pid)
	if err != nil {
		t.Fatal(err)
	}
	o.request.ParentProcess = p.identity
	if err = atomicJSON(filepath.Join(o.request.OperationDirectory, "request.json"), o.request); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cmd.Process.Kill(); _ = cmd.Wait() })
}
func TestComponentHelperOnlyCommitsWithoutStoppingEitherProcess(t *testing.T) {
	o := componentFixture(t, "update-helper")
	prepareComponentFixture(t, o)
	before, err := os.Stat(componentImage(o.request, o.request.Current["daemon"]))
	if err != nil {
		t.Fatal(err)
	}
	if err = o.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
		t.Fatal("helper-only activation stopped live app/runtime")
	}
	s, err := o.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if s.Generation != 1 || s.Trial != nil || !reflect.DeepEqual(s.Active, o.request.TargetComponents) || s.Previous != nil {
		t.Fatal("incoherent committed selection")
	}
	after, err := os.Stat(componentImage(o.request, o.request.Current["daemon"]))
	if err != nil {
		t.Fatal(err)
	}
	if !os.SameFile(before, after) || before.ModTime() != after.ModTime() {
		t.Fatal("unchanged daemon bytes were touched")
	}
	if !reflect.DeepEqual(o.journal.Status.ObservedComponents, componentIdentities(o.request.TargetComponents)) {
		t.Fatal("commit omitted actual component identities")
	}
}
func TestComponentDaemonOnlyRetainsGUIAndObservesIndependentIdentity(t *testing.T) {
	o := componentFixture(t, "daemon")
	startRetainedComponentGUI(t, o)
	prepareComponentFixture(t, o)
	handoffComponentFixture(t, o)
	if err := o.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	if o.journal.Status.Phase != "committed" || exited(o.request.ParentProcess) {
		t.Fatal("daemon-only did not retain exact GUI")
	}
	if o.journal.Health.Components["app"].Version != "1.0.0" || o.journal.Health.Components["daemon"].Version != "2.0.0" {
		t.Fatal("component identities were incorrectly aligned")
	}
}
func TestComponentGUIRestartLaunchesSelectedImageAndCommits(t *testing.T) {
	o := componentFixture(t, "app")
	prepareComponentFixture(t, o)
	handoffComponentFixture(t, o)
	if err := o.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	if o.journal.Health == nil || o.journal.Health.GUIProcess != o.journal.Launched || o.journal.Health.Components["daemon"].Version != "1.0.0" {
		t.Fatal("GUI health did not bind independent selected process/components")
	}
}

func TestComponentLegacyHandoffCanActivateIndependentDaemonApp(t *testing.T) {
	for _, health := range []string{"ready", "wrong"} {
		t.Run(health, func(t *testing.T) {
			t.Setenv("PASEO_COMPONENT_MARK_APP", "1")
			t.Setenv("PASEO_COMPONENT_HEALTH", health)
			o := componentFixture(t, "app", "daemon")
			prepareComponentFixture(t, o)
			handoffComponentFixture(t, o)
			err := o.activateAndCheck()
			if health == "ready" {
				if err != nil || o.journal.Status.Phase != "committed" || o.journal.Health == nil ||
					o.journal.Health.GUIProcess != o.journal.Launched || o.journal.Health.DaemonProcess.PID != 0 ||
					len(o.journal.Health.RuntimeProcesses) != 0 {
					t.Fatalf("marked app did not commit without an owned daemon: phase=%s health=%+v error=%v", o.journal.Status.Phase, o.journal.Health, err)
				}
			} else if o.journal.Status.Phase != "rolledBack" || o.journal.Health == nil || o.journal.Health.DaemonProcess.PID == 0 {
				t.Fatalf("invalid app-only health did not restore legacy GUI and runtime: phase=%s health=%+v error=%v", o.journal.Status.Phase, o.journal.Health, err)
			}
		})
	}
}
func TestComponentFailedHealthRestoresCoherentSelection(t *testing.T) {
	for _, id := range []string{"app", "daemon"} {
		t.Run(id, func(t *testing.T) {
			o := componentFixture(t, id)
			t.Setenv("PASEO_COMPONENT_HEALTH", "wrong")
			if id == "daemon" {
				startRetainedComponentGUI(t, o)
			}
			prepareComponentFixture(t, o)
			handoffComponentFixture(t, o)
			if err := o.activateAndCheck(); err != nil {
				t.Fatal(err)
			}
			if o.journal.Status.Phase != "rolledBack" || o.journal.Status.RecoveryRequired {
				t.Fatalf("unsafe/incomplete rollback: %+v", o.journal.Status)
			}
			s, err := o.readSelection()
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(s, o.journal.Original) {
				t.Fatal("failed health did not restore whole original selection")
			}
			if id == "daemon" && exited(o.request.ParentProcess) {
				t.Fatal("daemon rollback killed retained GUI")
			}
		})
	}
}
func TestComponentNoChangeDoesNotCreateSelectionOrObjects(t *testing.T) {
	o := componentFixture(t)
	if err := runComponentRequest(filepath.Join(o.request.OperationDirectory, "request.json")); err != nil {
		t.Fatal(err)
	}
	var status componentStatus
	if _, err := readJSON(filepath.Join(o.request.OperationDirectory, "status.json"), &status); err != nil || status.Phase != "committed" {
		t.Fatal("no-change did not finish as committed no-op", err)
	}
	if _, err := os.Stat(filepath.Join(o.request.StoreDirectory, "selection.json")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("no-change created selection")
	}
	if _, err := os.Stat(filepath.Join(o.request.StoreDirectory, "objects")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("no-change staged objects")
	}
	if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
		t.Fatal("no-change stopped processes")
	}
}
func TestComponentRejectsABIMismatchCorruptionAndWrongRestartBeforeStop(t *testing.T) {
	for _, kind := range []string{"abi", "archive", "restart", "inventory", "reuse"} {
		t.Run(kind, func(t *testing.T) {
			o := componentFixture(t, "daemon")
			switch kind {
			case "abi":
				c := o.request.TargetComponents["daemon"]
				c.Component.Identity.ABIVersion = 99
				o.request.TargetComponents["daemon"] = c
			case "archive":
				mustWrite(t, o.request.Staged["daemon"], []byte("bad"), 0600)
			case "restart":
				o.request.RestartGroup = "none"
			case "inventory":
				mustWrite(t, o.request.InventoryPath, []byte("{}"), 0600)
			case "reuse":
				mustWrite(t, componentImage(o.request, o.request.Current["tailnet"]), []byte("bad reused image"), 0700)
			}
			err := validateComponentRequest(o.request, filepath.Join(o.request.OperationDirectory, "request.json"))
			if err == nil {
				err = o.prepare()
			}
			if err == nil {
				t.Fatal("invalid transaction accepted")
			}
			if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
				t.Fatal("preparation failure stopped unrelated runtime")
			}
		})
	}
}
func TestComponentOrphanTrialWithoutStopAckIsNeverCommittedOrReplayed(t *testing.T) {
	o := componentFixture(t, "daemon")
	prepareComponentFixture(t, o)
	s := o.journal.Original
	s.Trial = &componentTrial{o.request.OperationID, o.request.TargetComponents}
	if err := o.writeSelection(s); err != nil {
		t.Fatal(err)
	}
	o.journal.Step = "checking"
	o.stopTimeout = 30 * time.Millisecond
	if err := o.recover(); err == nil {
		t.Fatal("orphan trial claimed recovery without stop acknowledgment")
	}
	if !o.journal.Status.RecoveryRequired || o.journal.Status.Phase != "failed" {
		t.Fatal("orphan recovery not truthful")
	}
	actual, err := o.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(actual.Active, o.request.Current) || actual.Trial == nil {
		t.Fatal("orphan recovery mutated active selection or discarded trial evidence")
	}
	if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
		t.Fatal("orphan recovery killed unproven process")
	}
}
func TestComponentUnrecordedLaunchRequiresRecovery(t *testing.T) {
	o := componentFixture(t, "app")
	prepareComponentFixture(t, o)
	o.journal.Step = "trial"
	o.journal.LaunchPending = true
	if err := o.recover(); err == nil || !o.journal.Status.RecoveryRequired {
		t.Fatal("unrecorded launch guessed process state")
	}
}
func TestComponentEstablishedMissingSelectionFailsClosed(t *testing.T) {
	o := componentFixture(t)
	mustWrite(t, filepath.Join(o.request.StoreDirectory, "established"), []byte("{}"), 0600)
	if err := o.initialSelection(); err == nil {
		t.Fatal("missing established selection silently bootstrapped")
	}
}
func TestComponentStaleGenerationAndLeaseAreRejected(t *testing.T) {
	o := componentFixture(t, "update-helper")
	prepareComponentFixture(t, o)
	s := o.journal.Original
	s.Generation = 3
	if err := o.writeSelection(s); err != nil {
		t.Fatal(err)
	}
	if err := o.activateAndCheck(); err == nil {
		t.Fatal("stale transaction activated")
	}
	lease, err := acquireLease(filepath.Join(o.request.StoreDirectory, "lease"))
	if err != nil {
		t.Fatal(err)
	}
	defer lease.Close()
	if second, e := acquireLease(filepath.Join(o.request.StoreDirectory, "lease")); e == nil {
		second.Close()
		t.Fatal("second installation-store transaction acquired lease")
	}
}

func writeComponentFixtureDescriptor(t *testing.T, root string, c componentRef) {
	t.Helper()
	hash, err := componentFileHash(filepath.Join(root, "identity.json"))
	if err != nil {
		t.Fatal(err)
	}
	d := componentBuildDescriptor{SchemaVersion: 1, ID: c.ID, Target: c.Target, Identity: c.Identity, Executable: c.Executable, ExecutableSHA256: c.ExecutableSHA256, Requires: c.Requires, Licenses: []string{}, Dependencies: map[string]string{"identity.json": hash}}
	if os.Getenv("PASEO_COMPONENT_MARK_APP") == "1" && c.ID == "app" {
		if hash, err := componentFileHash(filepath.Join(root, independentDaemonMarker)); err == nil {
			d.Dependencies[independentDaemonMarker] = hash
		}
	}
	if err = atomicJSON(filepath.Join(root, "component-build-"+c.ID+".json"), d); err != nil {
		t.Fatal(err)
	}
}

func TestComponentSharedRuntimeAndTailnetChangesUseGUIRestart(t *testing.T) {
	o := componentFixture(t, "daemon", "tailnet", "update-helper")
	prepareComponentFixture(t, o)
	handoffComponentFixture(t, o)
	if o.request.RestartGroup != "gui" {
		t.Fatal("loaded tailnet change did not require GUI restart")
	}
	if err := o.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	if o.journal.Status.Phase != "committed" || o.journal.Health.Components["app"].Version != "1.0.0" || o.journal.Health.Components["tailnet"].Version != "2.0.0" {
		t.Fatal("shared runtime change lost independent identities")
	}
}

func TestComponentExistingImmutableObjectIsReusedWithoutWrites(t *testing.T) {
	o := componentFixture(t, "update-helper")
	prepareComponentFixture(t, o)
	image := componentImage(o.request, o.request.TargetComponents["update-helper"])
	receipt := filepath.Join(o.request.StoreDirectory, "receipts", o.request.TargetComponents["update-helper"].Component.Artifact.SHA256+".json")
	before, err := os.Stat(image)
	if err != nil {
		t.Fatal(err)
	}
	beforeReceipt, err := os.Stat(receipt)
	if err != nil {
		t.Fatal(err)
	}
	if err = o.stageObjects(); err != nil {
		t.Fatal(err)
	}
	after, err := os.Stat(image)
	if err != nil {
		t.Fatal(err)
	}
	afterReceipt, err := os.Stat(receipt)
	if err != nil {
		t.Fatal(err)
	}
	if !os.SameFile(before, after) || before.ModTime() != after.ModTime() || !os.SameFile(beforeReceipt, afterReceipt) || beforeReceipt.ModTime() != afterReceipt.ModTime() {
		t.Fatal("reused immutable object or receipt was rewritten")
	}
}

func TestComponentCorruptObjectResourceIsRejectedBeforeActivation(t *testing.T) {
	o := componentFixture(t, "daemon")
	prepareComponentFixture(t, o)
	resource := filepath.Join(componentRoot(o.request, o.request.TargetComponents["daemon"]), "identity.json")
	if err := os.Chmod(resource, 0600); err != nil {
		t.Fatal(err)
	}
	mustWrite(t, resource, []byte("{}"), 0600)
	if err := os.Chmod(resource, 0400); err != nil {
		t.Fatal(err)
	}
	if err := o.stageObjects(); err == nil {
		t.Fatal("corrupt reused resource was accepted")
	}
	if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
		t.Fatal("corrupt reuse stopped current runtime")
	}
}

func TestComponentInterruptedTrialRecoversWithRealCooperativeProcesses(t *testing.T) {
	o := componentFixture(t, "app")
	prepareComponentFixture(t, o)
	handoffComponentFixture(t, o)
	s := o.journal.Original
	s.Trial = &componentTrial{o.request.OperationID, o.request.TargetComponents}
	if err := o.writeSelection(s); err != nil {
		t.Fatal(err)
	}
	if err := o.launch(o.request.TargetComponents); err != nil {
		t.Fatal(err)
	}
	o.journal.Step = "checking"
	if err := o.phase("checking", "fixture interrupted trial", true); err != nil {
		t.Fatal(err)
	}
	if !waitUntil(o.healthTimeout, o.interval, func() bool { return o.checkHealth("health.json", o.request.TargetComponents) == nil }) {
		t.Fatal("real trial never became healthy")
	}
	if err := o.recover(); err != nil {
		t.Fatal(err)
	}
	if o.journal.Status.Phase != "rolledBack" || o.journal.Status.RecoveryRequired {
		t.Fatal("interrupted trial was replayed or falsely recovered")
	}
	actual, err := o.readSelection()
	if err != nil || !reflect.DeepEqual(actual, o.journal.Original) {
		t.Fatal("interrupted trial failed coherent restoration", err)
	}
}

func TestComponentTerminalRetryPinsCompleteRequestWithoutReplay(t *testing.T) {
	o := componentFixture(t)
	p := filepath.Join(o.request.OperationDirectory, "request.json")
	if err := runComponentRequest(p); err != nil {
		t.Fatal(err)
	}
	if err := runComponentRequest(p); err != nil {
		t.Fatal(err)
	}
	o.request.ServerID = "another-server"
	if err := atomicJSON(p, o.request); err != nil {
		t.Fatal(err)
	}
	if err := runComponentRequest(p); err == nil {
		t.Fatal("same UUID accepted a different request")
	}
	if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
		t.Fatal("terminal retry restarted runtime")
	}
}

func TestComponentJSONRejectsDuplicateSecurityKeys(t *testing.T) {
	p := filepath.Join(canonicalTemp(t), "request.json")
	mustWrite(t, p, []byte(`{"schemaVersion":1,"schemaVersion":2}`), 0600)
	var value map[string]any
	if _, err := readJSON(p, &value); err == nil {
		t.Fatal("ambiguous schemaVersion was accepted")
	}
}

func TestComponentHelperQueryRejectsIncorrectActualABI(t *testing.T) {
	o := componentFixture(t)
	c := o.request.Current["update-helper"]
	actual := c.Component.Identity
	actual.ABIVersion = 1
	root := componentRoot(o.request, c)
	if err := atomicJSON(filepath.Join(root, "identity.json"), actual); err != nil {
		t.Fatal(err)
	}
	writeComponentFixtureDescriptor(t, root, c.Component)
	if err := o.prepare(); err == nil {
		t.Fatal("selected helper claimed ABI differs from descriptor but was accepted")
	}
	if exited(o.request.ParentProcess) || exited(o.request.RuntimeProcesses[0].Process) {
		t.Fatal("helper ABI rejection stopped current processes")
	}
}

func TestComponentRollbackRefusalPreservesLiveGUIAndReportsRecovery(t *testing.T) {
	o := componentFixture(t, "app")
	t.Setenv("PASEO_COMPONENT_HEALTH", "refuse")
	o.healthTimeout = 300 * time.Millisecond
	o.stopTimeout = 100 * time.Millisecond
	prepareComponentFixture(t, o)
	handoffComponentFixture(t, o)
	if err := o.activateAndCheck(); err == nil {
		t.Fatal("uncooperative replacement was reported as rolled back")
	}
	if !o.journal.Status.RecoveryRequired || o.journal.Status.Phase != "failed" || exited(o.journal.Launched) {
		t.Fatal("rollback killed a live GUI or hid recovery requirement")
	}
	s, err := o.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if s.Trial == nil || !reflect.DeepEqual(s.Active, o.request.Current) {
		t.Fatal("unsafe rollback modified original coherent selection")
	}
}

func TestComponentSignedCohortPreservesCompiledTailnetIdentity(t *testing.T) {
	target := componentTarget{Components: map[string]componentRef{}, Packages: []componentPackage{}, AtomicGroups: [][]string{{"app", "tailnet"}}}
	for _, id := range componentIDs {
		target.Components[id] = componentRef{ID: id, Target: "macos-arm64", Identity: componentFixtureIdentity(id, "1.0.0"), Artifact: componentArtifact{OriginTag: "v1.0.0", SHA256: strings.Repeat("a", 64)}}
	}
	app := target.Components["app"]
	app.Identity = componentFixtureIdentity("app", "2.0.0")
	app.Artifact.OriginTag = "desktop-macos-v2.0.0"
	tailnet := target.Components["tailnet"]
	tailnet.Artifact = app.Artifact
	target.Components["app"], target.Components["tailnet"] = app, tailnet
	target.Packages = []componentPackage{{Target: "macos-arm64", Version: "2.0.0", SourceSHA: strings.Repeat("b", 40), Artifact: app.Artifact, Components: map[string]componentIdentity{"app": app.Identity, "tailnet": tailnet.Identity}}}
	if err := validateComponentOrigins(target, "macos-arm64", "2.0.0", strings.Repeat("b", 40)); err != nil {
		t.Fatal(err)
	}
	if target.Components["tailnet"].Identity.Version != "1.0.0" {
		t.Fatal("signed container repack relabeled compiled Go identity")
	}
	target.Packages = nil
	if err := validateComponentOrigins(target, "macos-arm64", "2.0.0", strings.Repeat("b", 40)); err == nil {
		t.Fatal("mixed-source cohort accepted without exact delivery package")
	}
}

func TestComponentArchivesRejectTraversalDuplicateAndEscapingLinks(t *testing.T) {
	for _, kind := range []string{"traversal", "absolute", "backslash", "duplicate", "symlink"} {
		t.Run(kind, func(t *testing.T) {
			base := canonicalTemp(t)
			archive := filepath.Join(base, "artifact.zip")
			f, err := os.OpenFile(archive, os.O_CREATE|os.O_WRONLY, 0600)
			if err != nil {
				t.Fatal(err)
			}
			z := zip.NewWriter(f)
			name := "file"
			switch kind {
			case "traversal":
				name = "../escape"
			case "absolute":
				name = "/escape"
			case "backslash":
				name = "a\\b"
			}
			h := &zip.FileHeader{Name: name}
			if kind == "symlink" {
				h.SetMode(os.ModeSymlink | 0700)
			} else {
				h.SetMode(0600)
			}
			w, err := z.CreateHeader(h)
			if err != nil {
				t.Fatal(err)
			}
			if kind == "symlink" {
				_, err = io.WriteString(w, "../../escape")
			} else {
				_, err = io.WriteString(w, "payload")
			}
			if err != nil {
				t.Fatal(err)
			}
			if kind == "duplicate" {
				duplicate := zip.FileHeader{Name: name}
				duplicate.SetMode(0600)
				w, err = z.CreateHeader(&duplicate)
				if err != nil {
					t.Fatal(err)
				}
				_, err = io.WriteString(w, "other")
				if err != nil {
					t.Fatal(err)
				}
			}
			if err = z.Close(); err != nil {
				t.Fatal(err)
			}
			if err = f.Close(); err != nil {
				t.Fatal(err)
			}
			if err = extractComponent(archive, "zip", filepath.Join(base, "object")); err == nil {
				t.Fatal("unsafe component archive accepted")
			}
			if _, err = os.Lstat(filepath.Join(base, "escape")); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("archive escaped its private object")
			}
		})
	}
}

// nextComponentFixture chains a second transaction on o's committed selection,
// replacing id with a freshly staged 3.0.0 object.
func nextComponentFixture(t *testing.T, o *componentOperation, id string) *componentOperation {
	t.Helper()
	committed, err := o.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	r := o.request
	base := filepath.Dir(r.OperationDirectory)
	r.OperationID = "22222222-2222-4222-8222-222222222222"
	r.OperationDirectory = filepath.Join(base, "operation-next")
	mustMkdir(t, r.OperationDirectory)
	r.InventoryPath = filepath.Join(r.OperationDirectory, "inventory.json")
	r.CurrentGeneration = committed.Generation
	r.Current = committed.Active
	r.TargetComponents = componentSet{}
	for k, v := range r.Current {
		r.TargetComponents[k] = v
	}
	c := r.Current[id].Component
	candidate := filepath.Join(base, "next-"+id)
	mustMkdir(t, candidate)
	fixtureBinary(t, filepath.Join(candidate, c.Executable))
	c.Identity = componentFixtureIdentity(id, "3.0.0")
	if err = atomicJSON(filepath.Join(candidate, "identity.json"), c.Identity); err != nil {
		t.Fatal(err)
	}
	if c.ExecutableSHA256, err = componentFileHash(filepath.Join(candidate, c.Executable)); err != nil {
		t.Fatal(err)
	}
	writeComponentFixtureDescriptor(t, candidate, c)
	archive := filepath.Join(r.OperationDirectory, id+".zip")
	zipComponentFixture(t, candidate, archive)
	archiveHash, err := componentFileHash(archive)
	if err != nil {
		t.Fatal(err)
	}
	st, err := os.Stat(archive)
	if err != nil {
		t.Fatal(err)
	}
	c.Artifact = componentArtifact{URL: "https://github.com/paseo/fixtures/releases/download/v3.0.0/" + id + ".zip", SHA256: archiveHash, Size: uint64(st.Size()), Format: "zip", OriginTag: "v3.0.0"}
	r.TargetComponents[id] = installedComponent{c, "objects/" + archiveHash}
	r.Staged = map[string]string{id: archive}
	r.RestartGroup = map[string]string{"app": "gui", "tailnet": "gui", "daemon": "daemon", "update-helper": "none"}[id]
	inventory, err := readComponentInventory(o.request.InventoryPath)
	if err != nil {
		t.Fatal(err)
	}
	inventory.Version, inventory.SourceSHA = "3.0.0", c.Identity.SourceSHA
	inventory.Targets[r.Target].Components[id] = c
	if err = atomicJSON(r.InventoryPath, inventory); err != nil {
		t.Fatal(err)
	}
	if r.ManifestSHA512, err = digest(r.InventoryPath); err != nil {
		t.Fatal(err)
	}
	if r.RestartGroup == "gui" {
		// The committed GUI and its daemon own the next handoff.
		r.ParentProcess, r.RuntimeProcesses = o.journal.Launched, o.journal.Health.RuntimeProcesses
		r.CurrentExecutable = componentImage(r, r.Current["app"])
	}
	if err = atomicJSON(filepath.Join(r.OperationDirectory, "request.json"), r); err != nil {
		t.Fatal(err)
	}
	me, err := inspectProcess(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	n := &componentOperation{request: r, journal: componentJournal{SchemaVersion: 2, Step: "preparing", Status: componentStatus{SchemaVersion: 2, OperationID: r.OperationID, HelperProcess: me.identity, Nonce: strings.Repeat("d", 64), RestartGroup: r.RestartGroup, ExpectedComponents: componentIdentities(r.TargetComponents)}}, parentTimeout: time.Second, healthTimeout: 2 * time.Second, stopTimeout: 2 * time.Second, interval: 10 * time.Millisecond}
	t.Cleanup(func() {
		killFixture(n.journal.Launched)
		for _, name := range []string{"health.json", "rollback-health.json"} {
			var h componentHealth
			if _, e := readJSON(filepath.Join(r.OperationDirectory, name), &h); e == nil {
				killFixture(h.GUIProcess)
				for _, binding := range h.RuntimeProcesses {
					killFixture(binding.Process)
				}
			}
		}
	})
	return n
}
func storeObjectPaths(r componentRequest, digest string) []string {
	return []string{filepath.Join(r.StoreDirectory, "objects", digest), filepath.Join(r.StoreDirectory, "archives", digest), filepath.Join(r.StoreDirectory, "receipts", digest+".json")}
}
func requireAbsent(t *testing.T, paths []string) {
	t.Helper()
	for _, p := range paths {
		if _, err := os.Lstat(p); !errors.Is(err, os.ErrNotExist) {
			t.Fatal("superseded store entry survived:", p, err)
		}
	}
}

func TestComponentCommitDropsSupersededSetAndLegacyPrevious(t *testing.T) {
	o := componentFixture(t, "update-helper")
	prepareComponentFixture(t, o)
	if err := o.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	n := nextComponentFixture(t, o, "update-helper")
	superseded := n.request.Current["update-helper"]
	// An install committed by an older helper still names a previous set and
	// holds an orphaned sealed object; an interrupted payload belongs to its
	// own operation and must survive maintenance.
	legacy, err := o.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	legacy.Previous = o.request.Current
	if err = o.writeSelection(legacy); err != nil {
		t.Fatal(err)
	}
	orphan := storeObjectPaths(n.request, strings.Repeat("0", 64))
	mustMkdir(t, filepath.Join(orphan[0], "nested"))
	mustWrite(t, filepath.Join(orphan[0], "nested", "file"), []byte("old"), 0400)
	for _, p := range []string{filepath.Join(orphan[0], "nested"), orphan[0]} {
		if err = os.Chmod(p, 0500); err != nil {
			t.Fatal(err)
		}
	}
	mustWrite(t, orphan[1], []byte("old"), 0400)
	mustWrite(t, orphan[2], []byte("{}"), 0600)
	interrupted := filepath.Join(n.request.StoreDirectory, "objects", ".stage-33333333-3333-4333-8333-333333333333-daemon")
	mustMkdir(t, interrupted)

	prepareComponentFixture(t, n)
	s, err := n.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if s.Previous != nil || s.Generation != 1 || !reflect.DeepEqual(s.Active, n.request.Current) {
		t.Fatal("maintenance kept the legacy previous set or changed the active selection")
	}
	requireAbsent(t, orphan)
	if err = verifyObject(n.request, superseded); err != nil {
		t.Fatal("active object was pruned before commit:", err)
	}

	if err = n.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	s, err = n.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if n.journal.Status.Phase != "committed" || s.Generation != 2 || s.Previous != nil || !reflect.DeepEqual(s.Active, n.request.TargetComponents) {
		t.Fatal("commit kept a previous set")
	}
	requireAbsent(t, storeObjectPaths(n.request, superseded.Component.Artifact.SHA256))
	if err = verifyObject(n.request, n.request.TargetComponents["update-helper"]); err != nil {
		t.Fatal(err)
	}
	if _, err = os.Lstat(interrupted); err != nil {
		t.Fatal("pruning removed another operation's interrupted payload:", err)
	}
	if err = verifyObject(n.request, n.request.Current["daemon"]); err != nil {
		t.Fatal("bootstrap installation was touched:", err)
	}
}

func TestComponentRollbackBeforeCommitRestoresSupersededObjects(t *testing.T) {
	o := componentFixture(t, "app")
	prepareComponentFixture(t, o)
	handoffComponentFixture(t, o)
	if err := o.activateAndCheck(); err != nil || o.journal.Status.Phase != "committed" {
		t.Fatal("first update did not commit", err)
	}
	n := nextComponentFixture(t, o, "app")
	t.Setenv("PASEO_COMPONENT_HEALTH", "wrong")
	prepareComponentFixture(t, n)
	handoffComponentFixture(t, n)
	if err := n.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	if n.journal.Status.Phase != "rolledBack" || n.journal.Status.RecoveryRequired {
		t.Fatalf("unsafe/incomplete rollback: %+v", n.journal.Status)
	}
	s, err := n.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(s, n.journal.Original) || !reflect.DeepEqual(s.Active, n.request.Current) {
		t.Fatal("rollback did not restore the committed store selection")
	}
	// Rollback health already proved the restored app object ran; it and the
	// unhealthy candidate both remain until a later transaction's maintenance.
	for _, c := range []installedComponent{n.request.Current["app"], n.request.TargetComponents["app"]} {
		if err = verifyObject(n.request, c); err != nil {
			t.Fatal(err)
		}
	}
}

func TestComponentPruneFailureDoesNotFailCommit(t *testing.T) {
	o := componentFixture(t, "update-helper")
	prepareComponentFixture(t, o)
	orphan := storeObjectPaths(o.request, strings.Repeat("0", 64))
	mustMkdir(t, orphan[0])
	objects := filepath.Dir(orphan[0])
	if err := os.Chmod(objects, 0500); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(objects, 0700) })
	if err := o.activateAndCheck(); err != nil {
		t.Fatal(err)
	}
	s, err := o.readSelection()
	if err != nil {
		t.Fatal(err)
	}
	if o.journal.Status.Phase != "committed" || o.journal.Status.RecoveryRequired || !strings.Contains(o.journal.Status.Message, "could not be removed") || !reflect.DeepEqual(s.Active, o.request.TargetComponents) {
		t.Fatalf("prune failure changed the committed result: %+v", o.journal.Status)
	}
	if _, err = os.Lstat(orphan[0]); err != nil {
		t.Fatal("undeletable object vanished:", err)
	}
}
