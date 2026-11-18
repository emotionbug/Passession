package main

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"reflect"
	"regexp"
	"runtime"
	"strings"
	"time"
)

// Release builders stamp these per-component values, never the collection's
// identity for an unchanged helper. Unstamped developer helpers cannot activate.
var componentVersion = "development"
var componentSourceSHA = ""
var componentInputHash = ""

const componentHelperABI = 2

var componentIDs = []string{"app", "daemon", "tailnet", "update-helper"}
var appComponentIDs = [...]string{"app", "tailnet", "update-helper"}
var sha256Pattern = regexp.MustCompile(`^[0-9a-f]{64}$`)
var sourcePattern = regexp.MustCompile(`^[0-9a-f]{40}$`)
var versionPattern = regexp.MustCompile(`^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$`)
var componentAssetPattern = regexp.MustCompile(`^[0-9A-Za-z_.+-]{1,255}$`)
var componentRepositoryPartPattern = regexp.MustCompile(`^[0-9A-Za-z_.-]{1,100}$`)

type componentIdentity struct {
	Version    string `json:"version"`
	SourceSHA  string `json:"sourceSha"`
	InputHash  string `json:"inputHash"`
	ABIVersion uint32 `json:"abiVersion"`
}
type componentArtifact struct {
	URL       string `json:"url"`
	SHA256    string `json:"sha256"`
	Size      uint64 `json:"size"`
	Format    string `json:"format"`
	OriginTag string `json:"originTag"`
}
type componentRef struct {
	ID               string            `json:"id"`
	Target           string            `json:"target"`
	Identity         componentIdentity `json:"identity"`
	Artifact         componentArtifact `json:"artifact"`
	Executable       string            `json:"executable"`
	ExecutableSHA256 string            `json:"executableSha256"`
	Requires         map[string]uint32 `json:"requires"`
}
type installedComponent struct {
	Component componentRef `json:"component"`
	Root      string       `json:"root"`
}
type componentSet map[string]installedComponent

type componentPackage struct {
	Target     string                       `json:"target"`
	Version    string                       `json:"version"`
	SourceSHA  string                       `json:"sourceSha"`
	Artifact   componentArtifact            `json:"artifact"`
	Components map[string]componentIdentity `json:"components"`
}
type componentTarget struct {
	Components   map[string]componentRef `json:"components"`
	Packages     []componentPackage      `json:"packages"`
	AtomicGroups [][]string              `json:"atomicGroups"`
}
type componentInventory struct {
	SchemaVersion int                        `json:"schemaVersion"`
	Version       string                     `json:"version"`
	SourceSHA     string                     `json:"sourceSha"`
	Channel       string                     `json:"channel"`
	Targets       map[string]componentTarget `json:"targets"`
}
type bootstrapComponents struct {
	SchemaVersion int                     `json:"schemaVersion"`
	Target        string                  `json:"target"`
	Components    map[string]componentRef `json:"components"`
	Roots         map[string]string       `json:"roots"`
	Packages      []componentPackage      `json:"packages"`
}
type componentTrial struct {
	OperationID string       `json:"operationId"`
	Components  componentSet `json:"components"`
}

// Previous is kept only for wire compatibility: commits write null and the next
// transaction clears a legacy value. Rollback uses the journal's original.
type componentSelection struct {
	SchemaVersion  int             `json:"schemaVersion"`
	InstallationID string          `json:"installationId"`
	BaseRoot       string          `json:"baseRoot"`
	Target         string          `json:"target"`
	Generation     uint64          `json:"generation"`
	Active         componentSet    `json:"active"`
	Previous       componentSet    `json:"previous"`
	Trial          *componentTrial `json:"trial"`
}
type componentRequest struct {
	SchemaVersion      int               `json:"schemaVersion"`
	Mode               string            `json:"mode"`
	OperationID        string            `json:"operationId"`
	OperationDirectory string            `json:"operationDirectory"`
	StoreDirectory     string            `json:"storeDirectory"`
	BaseRoot           string            `json:"baseRoot"`
	InstallationID     string            `json:"installationId"`
	Target             string            `json:"target"`
	InventoryPath      string            `json:"inventoryPath"`
	ManifestSHA512     string            `json:"manifestSha512"`
	CurrentGeneration  uint64            `json:"currentGeneration"`
	Current            componentSet      `json:"current"`
	TargetComponents   componentSet      `json:"targetComponents"`
	Staged             map[string]string `json:"staged"`
	RestartGroup       string            `json:"restartGroup"`
	CurrentExecutable  string            `json:"currentExecutable"`
	ParentProcess      processIdentity   `json:"parentProcess"`
}
type componentStatus struct {
	SchemaVersion      int                          `json:"schemaVersion"`
	OperationID        string                       `json:"operationId"`
	Phase              string                       `json:"phase"`
	Ready              bool                         `json:"ready"`
	UpdatedAt          string                       `json:"updatedAt"`
	Message            string                       `json:"message"`
	RecoveryRequired   bool                         `json:"recoveryRequired"`
	HelperProcess      processIdentity              `json:"helperProcess"`
	Nonce              string                       `json:"nonce"`
	RestartGroup       string                       `json:"restartGroup"`
	ExpectedComponents map[string]componentIdentity `json:"expectedComponents"`
	ObservedComponents map[string]componentIdentity `json:"observedComponents,omitempty"`
}
type componentHealth struct {
	SchemaVersion int                          `json:"schemaVersion"`
	OperationID   string                       `json:"operationId"`
	Nonce         string                       `json:"nonce"`
	Components    map[string]componentIdentity `json:"components"`
	GUIProcess    processIdentity              `json:"guiProcess"`
}
type componentLeaseOwner struct {
	SchemaVersion      int             `json:"schemaVersion"`
	OperationID        string          `json:"operationId"`
	OperationDirectory string          `json:"operationDirectory"`
	HelperProcess      processIdentity `json:"helperProcess"`
	Nonce              string          `json:"nonce"`
	Generation         uint64          `json:"generation"`
}
type componentJournal struct {
	SchemaVersion    int                `json:"schemaVersion"`
	RequestDigest    string             `json:"requestDigest"`
	Status           componentStatus    `json:"status"`
	Step             string             `json:"step"`
	Original         componentSelection `json:"original"`
	Launched         processIdentity    `json:"launched"`
	LaunchPending    bool               `json:"launchPending"`
	Health           *componentHealth   `json:"health,omitempty"`
}
type componentOperation struct {
	request                                             componentRequest
	journal                                             componentJournal
	parentTimeout, healthTimeout, stopTimeout, interval time.Duration
}

func helperIdentity() any {
	return struct {
		SchemaVersion int               `json:"schemaVersion"`
		ID            string            `json:"id"`
		Identity      componentIdentity `json:"identity"`
	}{1, "update-helper", componentIdentity{componentVersion, componentSourceSHA, componentInputHash, componentHelperABI}}
}
func componentIdentities(set componentSet) map[string]componentIdentity {
	out := make(map[string]componentIdentity, len(set))
	for id, c := range set {
		out[id] = c.Component.Identity
	}
	return out
}
func validComponentID(id string) bool {
	for _, known := range componentIDs {
		if id == known {
			return true
		}
	}
	return false
}
func relativeComponentPath(p string) bool {
	return p != "" && len(p) <= 1024 && p != "." && p != ".." && path.Clean(p) == p && !strings.HasPrefix(p, "../") && !strings.HasPrefix(p, "/") && !strings.ContainsAny(p, "\\:\x00\r\n")
}
func validComponentIdentity(id string, v componentIdentity) bool {
	abi := uint32(1)
	if id == "update-helper" {
		abi = componentHelperABI
	}
	return validComponentVersion(v.Version) && sourcePattern.MatchString(v.SourceSHA) && sha256Pattern.MatchString(v.InputHash) && v.ABIVersion == abi
}
func validateComponentRef(c componentRef, id, target string) error {
	if !validComponentID(id) || c.ID != id || c.Target != target || !validComponentIdentity(id, c.Identity) || !relativeComponentPath(c.Executable) || !sha256Pattern.MatchString(c.ExecutableSHA256) {
		return errors.New("invalid component identity, ABI, or executable")
	}
	a := c.Artifact
	u, err := url.Parse(a.URL)
	if err != nil || u.Scheme != "https" || u.Host != "github.com" || u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.RawPath != "" || !sha256Pattern.MatchString(a.SHA256) || a.Size == 0 || a.Size > 8<<30 || (a.Format != "zip" && a.Format != "tar.gz") {
		return errors.New("invalid component artifact")
	}
	parts := strings.Split(strings.TrimPrefix(u.Path, "/"), "/")
	if len(parts) != 6 || parts[0] == "" || parts[1] == "" || parts[2] != "releases" || parts[3] != "download" || parts[4] != a.OriginTag || a.OriginTag == "" || parts[5] == "" || strings.ContainsAny(a.OriginTag, "\\\x00\r\n") {
		return errors.New("artifact must pin an original GitHub release asset")
	}
	if !componentRepositoryPartPattern.MatchString(parts[0]) || !componentRepositoryPartPattern.MatchString(parts[1]) || parts[0] == "." || parts[0] == ".." || parts[1] == "." || parts[1] == ".." || !componentAssetPattern.MatchString(parts[5]) || parts[5] == "." || parts[5] == ".." {
		return errors.New("unsafe component asset URL path")
	}
	if _, err := componentOriginVersion(a.OriginTag); err != nil {
		return err
	}
	for dependency, abi := range c.Requires {
		if !validComponentID(dependency) || dependency == id || abi == 0 {
			return errors.New("invalid component ABI requirement")
		}
	}
	return nil
}
func validateComponentSet(set componentSet, r componentRequest) error {
	if len(set) != len(componentIDs) {
		return errors.New("component selection must be complete")
	}
	for _, id := range componentIDs {
		v, ok := set[id]
		if !ok {
			return errors.New("missing component")
		}
		if err := validateComponentRef(v.Component, id, r.Target); err != nil {
			return fmt.Errorf("%s: %w", id, err)
		}
		if v.Root != "base" && !(strings.HasPrefix(v.Root, "base/") && relativeComponentPath(strings.TrimPrefix(v.Root, "base/"))) && v.Root != "objects/"+v.Component.Artifact.SHA256 {
			return errors.New("component root is not digest bound")
		}
		for dependency, abi := range v.Component.Requires {
			if set[dependency].Component.Identity.ABIVersion != abi {
				return errors.New("incompatible component dependency ABI")
			}
		}
	}
	return nil
}
func componentRoot(r componentRequest, c installedComponent) string {
	if c.Root == "base" {
		return r.BaseRoot
	}
	if strings.HasPrefix(c.Root, "base/") {
		return filepath.Join(r.BaseRoot, filepath.FromSlash(strings.TrimPrefix(c.Root, "base/")))
	}
	return filepath.Join(r.StoreDirectory, filepath.FromSlash(c.Root))
}
func componentImage(r componentRequest, c installedComponent) string {
	return filepath.Join(componentRoot(r, c), filepath.FromSlash(c.Component.Executable))
}
func installationKey(root string) string {
	if runtime.GOOS == "windows" {
		root = strings.ToLower(normalizedWindowsPath(root))
	}
	h := sha256.Sum256([]byte(root))
	return hex.EncodeToString(h[:])
}
func validateComponentRequest(r componentRequest, requestPath string) error {
	if r.SchemaVersion != 2 || r.Mode != "componentTransaction" || !uuidPattern.MatchString(r.OperationID) || !digestPattern.MatchString(r.ManifestSHA512) || r.InstallationID != installationKey(r.BaseRoot) || r.CurrentGeneration == ^uint64(0) {
		return errors.New("invalid component transaction identity")
	}
	if r.ParentProcess.PID <= 1 || r.ParentProcess.BirthToken == "" || len(r.ParentProcess.BirthToken) > 256 {
		return errors.New("invalid owning GUI process")
	}
	prefix := map[string]string{"linux": "linux-", "windows": "windows-", "darwin": "macos-"}[runtime.GOOS]
	arch := map[string]string{"amd64": "x64", "arm64": "arm64"}[runtime.GOARCH]
	if prefix == "" || arch == "" || r.Target != prefix+arch {
		return errors.New("component target is not this desktop platform/architecture")
	}
	if requestPath != filepath.Join(r.OperationDirectory, "request.json") || !within(r.OperationDirectory, r.InventoryPath) || len(r.Staged) > len(appComponentIDs) {
		return errors.New("transaction paths are not bound")
	}
	for _, p := range []string{r.OperationDirectory, r.StoreDirectory, r.BaseRoot, r.InventoryPath} {
		if err := cleanPath(p); err != nil {
			return err
		}
	}
	if !filepath.IsAbs(r.CurrentExecutable) || filepath.Clean(r.CurrentExecutable) != r.CurrentExecutable || strings.ContainsAny(r.CurrentExecutable, "\x00\r\n") {
		return errors.New("invalid current executable path")
	}
	for _, p := range []string{r.OperationDirectory, r.StoreDirectory} {
		if err := privatePath(p, true); err != nil {
			return err
		}
	}
	if err := privatePath(requestPath, false); err != nil {
		return err
	}
	if err := privatePath(r.InventoryPath, false); err != nil {
		return err
	}
	if filepath.Dir(r.BaseRoot) == r.BaseRoot || within(r.BaseRoot, r.StoreDirectory) || within(r.StoreDirectory, r.BaseRoot) || samePath(r.BaseRoot, r.StoreDirectory) {
		return errors.New("component storage overlaps the installed application")
	}
	if err := validateComponentSet(r.Current, r); err != nil {
		return err
	}
	if err := validateComponentSet(r.TargetComponents, r); err != nil {
		return err
	}
	if !reflect.DeepEqual(r.Current["daemon"], r.TargetComponents["daemon"]) {
		return errors.New("desktop transactions cannot change the daemon component")
	}
	if !samePath(componentImage(r, r.Current["app"]), r.CurrentExecutable) {
		st, err := os.Lstat(r.BaseRoot)
		if runtime.GOOS != "linux" || err != nil || !st.Mode().IsRegular() {
			return errors.New("parent is not selected app image")
		}
	}
	changed := 0
	group := "none"
	for _, id := range appComponentIDs {
		if reflect.DeepEqual(r.Current[id], r.TargetComponents[id]) {
			continue
		}
		changed++
		if r.TargetComponents[id].Root != "objects/"+r.TargetComponents[id].Component.Artifact.SHA256 {
			return errors.New("changed component must use immutable object")
		}
		archive, ok := r.Staged[id]
		if !ok || !within(r.OperationDirectory, archive) {
			return errors.New("changed component has no private staged archive")
		}
		if err := privatePath(archive, false); err != nil {
			return err
		}
		if id == "app" || id == "tailnet" {
			group = "gui"
		}
	}
	if changed != len(r.Staged) || group != r.RestartGroup {
		return errors.New("staged set or restart group does not match exact changes")
	}
	_, appStaged := r.Staged["app"]
	_, tailnetStaged := r.Staged["tailnet"]
	if runtime.GOOS == "darwin" && appStaged != tailnetStaged {
		return errors.New("signed macOS app/tailnet cohort must change together")
	}
	return nil
}
func boundLive(process processIdentity, image string) error {
	p, err := inspectProcess(process.PID)
	if err != nil {
		return err
	}
	if p.identity != process || !samePath(p.executable, image) {
		return errors.New("process birth/image binding changed")
	}
	return ownedProcess(p)
}
func (o *componentOperation) readSelection() (componentSelection, error) {
	var s componentSelection
	p := filepath.Join(o.request.StoreDirectory, "selection.json")
	if err := privatePath(p, false); err != nil {
		return s, err
	}
	_, err := readJSON(p, &s)
	if err != nil {
		return s, err
	}
	if s.SchemaVersion != 1 || s.InstallationID != o.request.InstallationID || s.BaseRoot != o.request.BaseRoot || s.Target != o.request.Target {
		return s, errors.New("selection installation binding mismatch")
	}
	if err = validateComponentSet(s.Active, o.request); err != nil {
		return s, err
	}
	// A legacy previous set is never selected or restored; it is only cleared.
	if s.Trial != nil {
		if !uuidPattern.MatchString(s.Trial.OperationID) {
			return s, errors.New("invalid trial identity")
		}
		if err = validateComponentSet(s.Trial.Components, o.request); err != nil {
			return s, err
		}
	}
	return s, nil
}
func (o *componentOperation) writeSelection(s componentSelection) error {
	marker := filepath.Join(o.request.StoreDirectory, "established")
	if _, err := os.Lstat(marker); errors.Is(err, os.ErrNotExist) {
		if err = atomicJSON(marker, map[string]any{"schemaVersion": 1, "installationId": o.request.InstallationID}); err != nil {
			return err
		}
	} else if err != nil {
		return err
	} else if err = privatePath(marker, false); err != nil {
		return err
	}
	return atomicJSON(filepath.Join(o.request.StoreDirectory, "selection.json"), s)
}
func (o *componentOperation) initialSelection() error {
	s, err := o.readSelection()
	if errors.Is(err, os.ErrNotExist) {
		if _, markerErr := os.Lstat(filepath.Join(o.request.StoreDirectory, "established")); !errors.Is(markerErr, os.ErrNotExist) {
			return errors.New("established component selection is missing")
		}
		if o.request.CurrentGeneration != 0 {
			return errors.New("missing established component selection")
		}
		for _, c := range o.request.Current {
			if c.Root != "base" && !strings.HasPrefix(c.Root, "base/") {
				return errors.New("initial selection is not bootstrap")
			}
		}
		s = componentSelection{SchemaVersion: 1, InstallationID: o.request.InstallationID, BaseRoot: o.request.BaseRoot, Target: o.request.Target, Active: o.request.Current}
	} else if err != nil {
		return err
	}
	if s.Trial != nil {
		return errors.New("an orphan trial requires its original operation recovery")
	}
	if s.Generation != o.request.CurrentGeneration || !reflect.DeepEqual(s.Active, o.request.Current) {
		return errors.New("stale current selection")
	}
	if s.Previous != nil {
		// Committed selections no longer keep the superseded set.
		s.Previous = nil
		if err = o.writeSelection(s); err != nil {
			return err
		}
	}
	o.journal.Original = s
	return nil
}
