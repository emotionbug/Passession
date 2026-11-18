package main

import (
	"crypto/rand"
	"crypto/sha512"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"time"
)

func runComponentRequest(path string) error {
	var r componentRequest
	raw, err := readJSON(path, &r)
	if err != nil {
		return err
	}
	if err = validateComponentRequest(r, path); err != nil {
		return err
	}
	operationLease, err := acquireLease(filepath.Join(r.OperationDirectory, "lease"))
	if err != nil {
		return err
	}
	defer operationLease.Close()
	installationLease, err := acquireInstallationLease(r.BaseRoot)
	if err != nil {
		return err
	}
	defer installationLease.Close()
	storeLease, err := acquireLease(filepath.Join(r.StoreDirectory, "lease"))
	if err != nil {
		return err
	}
	defer storeLease.Close()
	h := sha512.Sum512(raw)
	o := componentOperation{request: r, parentTimeout: 120 * time.Second, healthTimeout: 120 * time.Second, stopTimeout: 120 * time.Second, interval: 200 * time.Millisecond}
	journalPath := filepath.Join(r.OperationDirectory, "journal.json")
	me, err := inspectProcess(os.Getpid())
	if err != nil {
		return err
	}
	if _, err = os.Lstat(journalPath); err == nil {
		if err = privatePath(journalPath, false); err != nil {
			return err
		}
		if _, err = readJSON(journalPath, &o.journal); err != nil {
			return err
		}
		if o.journal.SchemaVersion != 2 || o.journal.RequestDigest != hex.EncodeToString(h[:]) || o.journal.Status.OperationID != r.OperationID || o.journal.Status.SchemaVersion != 2 || o.journal.Status.RestartGroup != r.RestartGroup || !reflect.DeepEqual(o.journal.Status.ExpectedComponents, componentIdentities(r.TargetComponents)) || !sha256Pattern.MatchString(o.journal.Status.Nonce) {
			return errors.New("component request does not match durable journal")
		}
		if terminal(o.journal.Status.Phase) {
			return atomicJSON(filepath.Join(r.OperationDirectory, "status.json"), o.journal.Status)
		}
		o.journal.Status.HelperProcess = me.identity
		if err = o.writeLeaseOwner(); err != nil {
			return err
		}
		return o.recover()
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	nonce := make([]byte, 32)
	if _, err = rand.Read(nonce); err != nil {
		return err
	}
	o.journal = componentJournal{SchemaVersion: 2, RequestDigest: hex.EncodeToString(h[:]), Step: "preparing", Status: componentStatus{SchemaVersion: 2, OperationID: r.OperationID, HelperProcess: me.identity, Nonce: hex.EncodeToString(nonce), RestartGroup: r.RestartGroup, ExpectedComponents: componentIdentities(r.TargetComponents)}}
	if err = o.phase("preparation", "Validating private component objects and exact component identities.", false); err != nil {
		return err
	}
	if err = o.prepare(); err != nil {
		return o.fail("Component preparation failed; running processes and active selection were not changed.", false, err)
	}
	if len(r.Staged) == 0 {
		o.journal.Status.ObservedComponents = componentIdentities(r.Current)
		return o.phase("committed", "No component changes; the active selection was left untouched.", false)
	}
	if err = o.writeLeaseOwner(); err != nil {
		return err
	}
	if r.RestartGroup != "none" {
		if err = o.phase("waitingForParent", "Components prepared; waiting for authenticated GUI handoff.", false); err != nil {
			return err
		}
		if !o.awaitHandoff() {
			return o.fail("GUI handoff did not complete; selection unchanged.", exited(r.ParentProcess), nil)
		}
	}
	return o.activateAndCheck()
}
func (o *componentOperation) persist() error {
	return atomicJSON(filepath.Join(o.request.OperationDirectory, "journal.json"), o.journal)
}
func (o *componentOperation) phase(phase, message string, recovery bool) error {
	if phase != "committed" {
		o.journal.Status.ObservedComponents = nil
	}
	o.journal.Status.Phase = phase
	o.journal.Status.Ready = phase == "waitingForParent"
	o.journal.Status.Message = message
	o.journal.Status.RecoveryRequired = recovery
	o.journal.Status.UpdatedAt = time.Now().UTC().Format(time.RFC3339Nano)
	if err := o.persist(); err != nil {
		return err
	}
	return atomicJSON(filepath.Join(o.request.OperationDirectory, "status.json"), o.journal.Status)
}
func (o *componentOperation) fail(message string, recovery bool, cause error) error {
	if err := o.phase("failed", message, recovery); err != nil {
		return err
	}
	if cause != nil {
		return cause
	}
	return errors.New(message)
}
func (o *componentOperation) writeLeaseOwner() error {
	return atomicJSON(filepath.Join(o.request.StoreDirectory, "lease-owner.json"), componentLeaseOwner{1, o.request.OperationID, o.request.OperationDirectory, o.journal.Status.HelperProcess, o.journal.Status.Nonce, o.request.CurrentGeneration})
}
func (o *componentOperation) prepare() error {
	r := o.request
	if err := boundLive(r.ParentProcess, r.CurrentExecutable); err != nil {
		return err
	}
	if !samePath(r.CurrentExecutable, componentImage(r, r.Current["app"])) {
		p, err := inspectProcess(r.ParentProcess.PID)
		if err != nil {
			return err
		}
		if runtime.GOOS != "linux" {
			return errors.New("parent app is not selected image")
		}
		if err = validateAppImageProcess(p, r.BaseRoot); err != nil {
			return err
		}
		if actual, e := componentFileHash(r.CurrentExecutable); e != nil || actual != r.Current["app"].Component.ExecutableSHA256 {
			return errors.New("mounted bootstrap app differs from selected object")
		}
	}
	if err := validatePinnedInventory(r); err != nil {
		return err
	}
	if err := o.initialSelection(); err != nil {
		return err
	}
	// Store maintenance: only the active set and this request's target remain.
	_ = o.pruneSuperseded(o.journal.Original.Active, r.TargetComponents)
	for _, c := range r.Current {
		if err := verifyObject(r, c); err != nil {
			return err
		}
	}
	if len(r.Staged) != 0 {
		if err := o.stageObjects(); err != nil {
			return err
		}
	}
	if err := queryComponentHelper(r, r.TargetComponents["update-helper"]); err != nil {
		return fmt.Errorf("selected helper identity query: %w", err)
	}
	return o.persist()
}
func (o *componentOperation) authorized(name string) bool {
	path := filepath.Join(o.request.OperationDirectory, name)
	if err := privatePath(path, false); err != nil {
		return false
	}
	var ack struct {
		SchemaVersion int    `json:"schemaVersion"`
		OperationID   string `json:"operationId"`
		Nonce         string `json:"nonce"`
	}
	_, err := readJSON(path, &ack)
	return err == nil && ack.SchemaVersion == 2 && ack.OperationID == o.request.OperationID && ack.Nonce == o.journal.Status.Nonce
}
func (o *componentOperation) handoffCompleted() bool {
	return o.authorized("handoff.json") && exited(o.request.ParentProcess)
}
func (o *componentOperation) awaitHandoff() bool {
	return waitUntil(o.parentTimeout, o.interval, o.handoffCompleted)
}
func (o *componentOperation) selectionUnchanged() error {
	s, err := o.readSelection()
	if errors.Is(err, os.ErrNotExist) && o.journal.Original.Generation == 0 {
		if _, markerErr := os.Lstat(filepath.Join(o.request.StoreDirectory, "established")); errors.Is(markerErr, os.ErrNotExist) {
			return nil
		}
		return errors.New("established selection disappeared")
	}
	if err != nil {
		return err
	}
	if !reflect.DeepEqual(s, o.journal.Original) {
		return errors.New("component selection changed during preparation")
	}
	return nil
}
func (o *componentOperation) activateAndCheck() error {
	// Recheck all immutable receipts/images immediately before the atomic pointer
	// change. Never overwrite an existing object, including unchanged components.
	if err := o.selectionUnchanged(); err != nil {
		return o.fail("Active selection changed; activation refused without overwriting another selection.", o.request.RestartGroup != "none", err)
	}
	for _, c := range o.request.TargetComponents {
		if err := verifyObject(o.request, c); err != nil {
			return o.beforeTrialFailure(err)
		}
	}
	if err := validatePinnedInventory(o.request); err != nil {
		return o.beforeTrialFailure(err)
	}
	if o.request.RestartGroup == "none" {
		if err := boundLive(o.request.ParentProcess, o.request.CurrentExecutable); err != nil {
			return o.fail("Owning GUI changed before helper activation.", false, err)
		}
		if err := queryComponentHelper(o.request, o.request.TargetComponents["update-helper"]); err != nil {
			return o.fail("Selected helper identity is not valid.", false, err)
		}
		return o.commit()
	}
	o.journal.Step = "activating"
	if err := o.phase("activating", "Activating a lease-bound trial component selection.", true); err != nil {
		return err
	}
	trial := o.journal.Original
	trial.Trial = &componentTrial{o.request.OperationID, o.request.TargetComponents}
	if err := o.writeSelection(trial); err != nil {
		return err
	}
	o.journal.Step = "trial"
	if err := o.phase("starting", "Starting the selected GUI.", true); err != nil {
		return err
	}
	if err := o.launch(o.request.TargetComponents); err != nil {
		return o.rollback(err)
	}
	o.journal.Step = "checking"
	if err := o.phase("checking", "Waiting for authenticated GUI component health.", true); err != nil {
		return err
	}
	if waitUntil(o.healthTimeout, o.interval, func() bool { return o.checkHealth("health.json", o.request.TargetComponents) == nil }) {
		return o.commit()
	}
	return o.rollback(errors.New("component health deadline expired"))
}
func (o *componentOperation) commit() error {
	if o.request.RestartGroup == "none" {
		if err := o.selectionUnchanged(); err != nil {
			return o.fail("Selection changed before commit; no unrelated selection overwritten.", false, err)
		}
	} else {
		actual, err := o.readSelection()
		expected := o.journal.Original
		expected.Trial = &componentTrial{o.request.OperationID, o.request.TargetComponents}
		if err != nil || !reflect.DeepEqual(actual, expected) {
			return o.fail("Trial changed before commit; no unrelated selection overwritten.", true, err)
		}
	}
	if o.request.RestartGroup == "none" {
		o.journal.Status.ObservedComponents = componentIdentities(o.request.TargetComponents)
	} else {
		if o.journal.Health == nil || !reflect.DeepEqual(o.journal.Health.Components, componentIdentities(o.request.TargetComponents)) {
			return errors.New("cannot commit without actual component health")
		}
		o.journal.Status.ObservedComponents = o.journal.Health.Components
	}
	// The complete healthy set, not four independently written pointers, is the
	// sole commit boundary. Record intent before replacement for crash recovery.
	o.journal.Step = "committing"
	if err := o.persist(); err != nil {
		return err
	}
	s := o.journal.Original
	s.Generation++
	s.Previous = nil
	s.Active = o.request.TargetComponents
	s.Trial = nil
	if err := o.writeSelection(s); err != nil {
		return err
	}
	return o.finishCommit("Expected component identities confirmed; coherent selection committed.")
}

// finishCommit drops the superseded set's store objects once the selection
// commit is durable. Deletion is best-effort and never undoes the commit.
func (o *componentOperation) finishCommit(message string) error {
	o.journal.Step = "committed"
	if o.pruneSuperseded(o.request.TargetComponents) != nil {
		message += " Some superseded component files could not be removed; the next update retries."
	}
	return o.phase("committed", message, false)
}

// pruneSuperseded keeps the selected sets plus any original image whose owning
// process is still live (or cannot be proven gone), then prunes the store.
func (o *componentOperation) pruneSuperseded(selected ...componentSet) error {
	r := o.request
	live := componentSet{}
	if !exited(r.ParentProcess) {
		live["app"], live["tailnet"] = r.Current["app"], r.Current["tailnet"]
	}
	err := pruneComponentStore(r, append(selected, live)...)
	if err != nil {
		fmt.Fprintln(os.Stderr, "Desktop update helper: warning: cannot remove superseded component files:", err)
	}
	return err
}
func (o *componentOperation) launch(set componentSet) error {
	image := componentImage(o.request, set["app"])
	if err := verifyObject(o.request, set["app"]); err != nil {
		return err
	}
	cmd := exec.Command(image)
	cmd.Dir = filepath.Dir(image)
	cmd.Env = o.launchEnvironment(os.Environ())
	detach(cmd)
	o.journal.LaunchPending = true
	if err := o.persist(); err != nil {
		return err
	}
	if err := cmd.Start(); err != nil {
		o.journal.LaunchPending = false
		_ = o.persist()
		return err
	}
	p, err := inspectProcess(cmd.Process.Pid)
	// Reap an exiting GUI so its exact exit is observable during rollback.
	// The detached child is free to outlive the helper on commit.
	go func() { _ = cmd.Wait() }()
	if err != nil {
		return err
	}
	o.journal.Launched = p.identity
	o.journal.LaunchPending = false
	return o.persist()
}
func (o *componentOperation) launchEnvironment(base []string) []string {
	r := o.request
	return append(scrubUpdateEnvironment(base, 5),
		"PASEO_UPDATE_OPERATION_DIRECTORY="+r.OperationDirectory,
		"PASEO_UPDATE_OPERATION_ID="+r.OperationID,
		"PASEO_UPDATE_NONCE="+o.journal.Status.Nonce,
		"PASEO_COMPONENT_STORE="+r.StoreDirectory,
		"PASEO_COMPONENT_BASE_ROOT="+r.BaseRoot,
	)
}

func scrubUpdateEnvironment(base []string, extra int) []string {
	out := make([]string, 0, len(base)+extra)
	for _, value := range base {
		key, _, _ := strings.Cut(value, "=")
		if runtime.GOOS == "windows" {
			key = strings.ToUpper(key)
		}
		if strings.HasPrefix(key, "PASEO_UPDATE_") || key == "PASEO_COMPONENT_STORE" || key == "PASEO_COMPONENT_BASE_ROOT" ||
			key == "APPDIR" || key == "APPIMAGE" || strings.HasPrefix(key, "APPIMAGE_") || strings.HasPrefix(key, "LD_") {
			continue
		}
		out = append(out, value)
	}
	return out
}

func (o *componentOperation) checkHealth(name string, set componentSet) error {
	p := filepath.Join(o.request.OperationDirectory, name)
	if err := privatePath(p, false); err != nil {
		return err
	}
	var h componentHealth
	if _, err := readJSON(p, &h); err != nil {
		return err
	}
	if h.SchemaVersion != 2 || h.OperationID != o.request.OperationID || h.Nonce != o.journal.Status.Nonce || h.GUIProcess != o.journal.Launched || len(h.Components) != len(appComponentIDs) {
		return errors.New("app-only health identity mismatch")
	}
	for _, id := range appComponentIDs {
		if observed, ok := h.Components[id]; !ok || observed != set[id].Component.Identity {
			return errors.New("app-only health component identity mismatch")
		}
	}
	if err := boundLive(h.GUIProcess, componentImage(o.request, set["app"])); err != nil {
		return err
	}
	return o.recordHealth(set, h)
}

func (o *componentOperation) recordHealth(set componentSet, h componentHealth) error {
	for _, c := range set {
		if err := verifyObject(o.request, c); err != nil {
			return err
		}
	}
	if err := queryComponentHelper(o.request, set["update-helper"]); err != nil {
		return err
	}
	// The journal records the whole verified selection for store integrity.
	// The unchanged daemon entry describes its bundled image, not a live process.
	h.Components = componentIdentities(set)
	o.journal.Health = &h
	return o.persist()
}
func (o *componentOperation) stopTrial() bool {
	if o.journal.LaunchPending || o.journal.Launched.PID <= 1 || o.journal.Launched.BirthToken == "" {
		return false
	}
	if err := atomicJSON(filepath.Join(o.request.OperationDirectory, "stop.json"), map[string]any{"schemaVersion": 2, "operationId": o.request.OperationID, "nonce": o.journal.Status.Nonce, "restartGroup": o.request.RestartGroup}); err != nil {
		return false
	}
	return waitUntil(o.stopTimeout, o.interval, func() bool {
		return o.authorized("stopped.json") && exited(o.journal.Launched)
	})
}
func (o *componentOperation) beforeTrialFailure(cause error) error {
	if o.request.RestartGroup == "none" {
		return o.fail("Prepared component changed; activation refused.", false, cause)
	}
	return o.restoreAndCheck(cause)
}
func (o *componentOperation) rollback(cause error) error {
	if !o.journal.LaunchPending && o.journal.Launched.PID == 0 && o.handoffCompleted() {
		return o.restoreAndCheck(cause)
	}
	if !o.stopTrial() {
		return o.fail("Trial GUI did not acknowledge a safe stop and exit; prior selection and immutable objects retained for recovery.", true, cause)
	}
	return o.restoreAndCheck(cause)
}
func (o *componentOperation) restoreAndCheck(cause error) error {
	actual, selectionErr := o.readSelection()
	if selectionErr == nil {
		trial := o.journal.Original
		trial.Trial = &componentTrial{o.request.OperationID, o.request.TargetComponents}
		if !reflect.DeepEqual(actual, o.journal.Original) && !reflect.DeepEqual(actual, trial) {
			return o.fail("Selection changed before restoration; unrelated selection preserved.", true, nil)
		}
	} else if !errors.Is(selectionErr, os.ErrNotExist) || o.journal.Original.Generation != 0 {
		return o.fail("Cannot prove selection safe to restore.", true, selectionErr)
	}
	// Only this transaction's nonce-bound stop command is removed. It must not
	// be replayed by the original GUI after a successful cooperative rollback.
	if err := os.Remove(filepath.Join(o.request.OperationDirectory, "stop.json")); err != nil && !errors.Is(err, os.ErrNotExist) {
		return o.fail("Cannot clear consumed cooperative stop request.", true, err)
	}
	for _, c := range o.journal.Original.Active {
		if err := verifyObject(o.request, c); err != nil {
			return o.fail("Previous component objects changed; unsafe restoration refused.", true, err)
		}
	}
	o.journal.Step = "restoring"
	if err := o.persist(); err != nil {
		return err
	}
	if err := o.writeSelection(o.journal.Original); err != nil {
		return err
	}
	o.journal.Health = nil
	o.journal.Launched = processIdentity{}
	o.journal.Step = "restored"
	if err := o.phase("rollingBack", "Previous coherent selection restored; waiting for previous GUI health.", true); err != nil {
		return err
	}
	if err := o.launch(o.journal.Original.Active); err != nil {
		return o.fail("Previous component selection restored but GUI relaunch failed.", true, err)
	}
	if !waitUntil(o.healthTimeout, o.interval, func() bool { return o.checkHealth("rollback-health.json", o.journal.Original.Active) == nil }) {
		return o.fail("Previous selection restored, but previous GUI readiness could not be confirmed.", true, cause)
	}
	o.journal.Step = "rolledBack"
	return o.phase("rolledBack", "Previous coherent component selection and GUI health restored; update not committed.", false)
}
func (o *componentOperation) recover() error {
	if o.journal.Original.SchemaVersion != 1 || o.journal.Original.InstallationID != o.request.InstallationID || o.journal.Original.BaseRoot != o.request.BaseRoot || o.journal.Original.Target != o.request.Target || o.journal.Original.Generation != o.request.CurrentGeneration || o.journal.Original.Trial != nil || !reflect.DeepEqual(o.journal.Original.Active, o.request.Current) {
		if o.journal.Step == "preparing" {
			return o.fail("Interrupted before completed preparation; no activation was authorized.", false, nil)
		}
		return o.fail("Recovery journal original selection is not bound to request.", true, nil)
	}
	if o.journal.LaunchPending {
		return o.fail("GUI launch interrupted before process identity was recorded; no process was killed or replacement replayed.", true, nil)
	}
	s, err := o.readSelection()
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return o.fail("Cannot validate interrupted selection.", true, err)
	}
	if o.journal.Step == "preparing" {
		if o.request.RestartGroup != "none" && o.handoffCompleted() {
			return o.restoreAndCheck(errors.New("interrupted after GUI handoff"))
		}
		recovery := o.request.RestartGroup != "none" && exited(o.request.ParentProcess)
		return o.fail("Interrupted before activation; original selection retained.", recovery, nil)
	}
	if err == nil && s.Generation == o.request.CurrentGeneration+1 && s.Trial == nil && reflect.DeepEqual(s.Active, o.request.TargetComponents) && s.Previous == nil && (o.journal.Step == "committing" || o.journal.Step == "committed") {
		// Commit was durable only after exact health (or a helper-only query). It
		// is never reinstalled/relaunched, even if the user has since closed GUI.
		if o.request.RestartGroup != "none" && o.journal.Health == nil {
			return o.fail("Commit lacks durable health evidence.", true, nil)
		}
		return o.finishCommit("Recovered durable component commit; no activation replayed.")
	}
	if o.request.RestartGroup == "none" && o.journal.Step == "committing" && ((err == nil && reflect.DeepEqual(s, o.journal.Original)) || (errors.Is(err, os.ErrNotExist) && o.journal.Original.Generation == 0)) {
		for _, c := range o.journal.Original.Active {
			if e := verifyObject(o.request, c); e != nil {
				return o.fail("Original component selection could not be proven.", true, e)
			}
		}
		if errors.Is(err, os.ErrNotExist) {
			if e := o.writeSelection(o.journal.Original); e != nil {
				return e
			}
		}
		return o.fail("Helper activation interrupted before commit; original selection retained without restarting any process.", false, nil)
	}
	if o.journal.Step == "restored" || o.journal.Step == "restoring" {
		if err != nil || !reflect.DeepEqual(s, o.journal.Original) {
			return o.fail("Interrupted restoration selection cannot be proven.", true, nil)
		}
		if o.checkHealth("rollback-health.json", o.journal.Original.Active) == nil {
			o.journal.Step = "rolledBack"
			return o.phase("rolledBack", "Recovered confirmed previous GUI.", false)
		}
		return o.fail("Previous selection restored but GUI restart was interrupted; no launch replayed.", true, nil)
	}
	if o.journal.Step == "activating" && ((err == nil && reflect.DeepEqual(s, o.journal.Original)) || (errors.Is(err, os.ErrNotExist) && o.journal.Original.Generation == 0)) {
		if !o.handoffCompleted() {
			return o.fail("Interrupted activation lacks proven GUI handoff.", true, nil)
		}
		return o.restoreAndCheck(errors.New("interrupted before durable trial activation"))
	}
	if err != nil || s.Trial == nil || s.Trial.OperationID != o.request.OperationID || !reflect.DeepEqual(s.Trial.Components, o.request.TargetComponents) || s.Generation != o.journal.Original.Generation || !reflect.DeepEqual(s.Active, o.journal.Original.Active) || !reflect.DeepEqual(s.Previous, o.journal.Original.Previous) {
		return o.fail("Interrupted trial no longer matches the journal; no selection changed.", true, nil)
	}
	// A recovered lease never silently re-authorizes a trial for launching. Stop
	// only via the original nonce and exact process acknowledgments, then restore.
	return o.rollback(errors.New("interrupted component transaction"))
}
