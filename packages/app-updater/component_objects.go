package main

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"crypto/sha512"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"sort"
	"strings"
	"time"
)

const componentExpandedLimit int64 = 16 << 30
const componentEntryLimit = 200000
const componentInventoryLimit = 2 << 20

type componentReceipt struct {
	SchemaVersion int                     `json:"schemaVersion"`
	Components    map[string]componentRef `json:"components"`
	Packages      []componentPackage      `json:"packages"`
	TreeSHA512    string                  `json:"treeSha512"`
}

func componentFileHash(path string) (string, error) {
	if err := regular(path); err != nil {
		return "", err
	}
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err = io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
func readComponentInventory(path string) (componentInventory, error) {
	var v componentInventory
	if err := regular(path); err != nil {
		return v, err
	}
	f, err := os.Open(path)
	if err != nil {
		return v, err
	}
	defer f.Close()
	b, err := io.ReadAll(io.LimitReader(f, componentInventoryLimit+1))
	if err != nil {
		return v, err
	}
	if len(b) > componentInventoryLimit {
		return v, errors.New("component inventory exceeds limit")
	}
	if err = unambiguousJSON(b); err != nil {
		return v, err
	}
	d := json.NewDecoder(bytes.NewReader(b))
	d.DisallowUnknownFields()
	if err = d.Decode(&v); err != nil {
		return v, err
	}
	if d.Decode(new(any)) != io.EOF {
		return v, errors.New("trailing component inventory JSON")
	}
	return v, nil
}
func validatePinnedInventory(r componentRequest) error {
	actual, err := digest(r.InventoryPath)
	if err != nil {
		return err
	}
	if actual != r.ManifestSHA512 {
		return errors.New("release inventory digest changed")
	}
	inventory, err := readComponentInventory(r.InventoryPath)
	if err != nil {
		return err
	}
	if inventory.SchemaVersion != 1 || !validComponentVersion(inventory.Version) || !sourcePattern.MatchString(inventory.SourceSHA) || (inventory.Channel != "stable" && inventory.Channel != "canary") {
		return errors.New("invalid release inventory identity")
	}
	target, ok := inventory.Targets[r.Target]
	if !ok || len(target.Components) != len(componentIDs) {
		return errors.New("inventory omits target component set")
	}
	if err = validateComponentOrigins(target, r.Target, inventory.Version, inventory.SourceSHA); err != nil {
		return err
	}
	changed := make(map[string]bool)
	for _, id := range componentIDs {
		c := target.Components[id]
		if err = validateComponentRef(c, id, r.Target); err != nil {
			return err
		}
		if !reflect.DeepEqual(r.Current[id], r.TargetComponents[id]) {
			changed[id] = true
			if !reflect.DeepEqual(r.TargetComponents[id].Component, c) {
				return errors.New("target component differs from pinned inventory")
			}
		}
	}
	for _, group := range target.AtomicGroups {
		if len(group) < 2 || len(group) > 4 {
			return errors.New("invalid inventory atomic group")
		}
		seen := make(map[string]bool)
		selected := false
		for _, id := range group {
			if !validComponentID(id) || seen[id] {
				return errors.New("invalid atomic group member")
			}
			seen[id] = true
			selected = selected || changed[id]
		}
		if selected {
			for _, id := range group {
				if !changed[id] {
					return errors.New("incomplete atomic component selection")
				}
			}
		}
	}
	return nil
}
func verifyBootstrapComponent(r componentRequest, c installedComponent) error {
	var b bootstrapComponents
	if _, err := readJSON(filepath.Join(r.BaseRoot, "components.json"), &b); err != nil {
		return err
	}
	if b.SchemaVersion != 1 || b.Target != r.Target || !reflect.DeepEqual(b.Components[c.Component.ID], c.Component) {
		return errors.New("bootstrap component descriptor mismatch")
	}
	root, ok := b.Roots[c.Component.ID]
	if !ok {
		return errors.New("bootstrap root missing")
	}
	expected := "base"
	if root != "." {
		if !relativeComponentPath(root) {
			return errors.New("invalid bootstrap root")
		}
		expected += "/" + root
	}
	if c.Root != expected {
		return errors.New("bootstrap component root mismatch")
	}
	if err := validateComponentDescriptor(componentRoot(r, c), c.Component, false); err != nil {
		return err
	}
	return verifyComponentImage(r, c)
}
func verifyComponentImage(r componentRequest, c installedComponent) error {
	image := componentImage(r, c)
	actual, err := componentFileHash(image)
	if err != nil {
		return err
	}
	if actual != c.Component.ExecutableSHA256 {
		return errors.New("component executable digest mismatch")
	}
	return nil
}
func verifyObject(r componentRequest, c installedComponent) error {
	if c.Root == "base" || strings.HasPrefix(c.Root, "base/") {
		return verifyBootstrapComponent(r, c)
	}
	root := componentRoot(r, c)
	if err := privatePath(root, true); err != nil {
		return err
	}
	var receipt componentReceipt
	p := filepath.Join(r.StoreDirectory, "receipts", c.Component.Artifact.SHA256+".json")
	if err := privatePath(p, false); err != nil {
		return err
	}
	if _, err := readJSON(p, &receipt); err != nil {
		return err
	}
	if receipt.SchemaVersion != 1 || !reflect.DeepEqual(receipt.Components[c.Component.ID], c.Component) || !digestPattern.MatchString(receipt.TreeSHA512) {
		return errors.New("immutable object receipt mismatch")
	}
	archive := filepath.Join(r.StoreDirectory, "archives", c.Component.Artifact.SHA256)
	if err := privatePath(archive, false); err != nil {
		return err
	}
	if err := verifyComponentArchive(archive, c.Component.Artifact); err != nil {
		return err
	}
	actual, err := componentTreeDigest(root, true)
	if err != nil {
		return err
	}
	if actual != receipt.TreeSHA512 {
		return errors.New("immutable component tree changed")
	}
	if err := validateComponentDescriptor(root, c.Component, true); err != nil {
		return err
	}
	return verifyComponentImage(r, c)
}
func verifyComponentArchive(p string, a componentArtifact) error {
	st, err := os.Lstat(p)
	if err != nil {
		return err
	}
	if st.Size() < 0 || uint64(st.Size()) != a.Size {
		return errors.New("component archive size mismatch")
	}
	actual, err := componentFileHash(p)
	if err != nil {
		return err
	}
	if actual != a.SHA256 {
		return errors.New("component archive digest mismatch")
	}
	return nil
}
func componentTreeDigest(root string, immutable bool) (string, error) {
	if st, err := os.Lstat(root); err != nil {
		return "", err
	} else if immutable && runtime.GOOS != "windows" && st.Mode().Perm()&0222 != 0 {
		return "", errors.New("component root is mutable")
	}
	var paths []string
	if err := filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if p != root {
			paths = append(paths, p)
		}
		if len(paths) > componentEntryLimit {
			return errors.New("component tree entry limit exceeded")
		}
		return nil
	}); err != nil {
		return "", err
	}
	sort.Slice(paths, func(i, j int) bool { return filepath.ToSlash(paths[i]) < filepath.ToSlash(paths[j]) })
	h := sha512.New()
	var total int64
	for _, p := range paths {
		st, err := os.Lstat(p)
		if err != nil {
			return "", err
		}
		if err = ownedPath(p); err != nil {
			return "", err
		}
		rel, err := filepath.Rel(root, p)
		if err != nil {
			return "", err
		}
		rel = filepath.ToSlash(rel)
		kind, size, executable := "d", int64(0), 0
		var link string
		if st.Mode()&os.ModeSymlink != 0 {
			kind = "l"
			link, err = os.Readlink(p)
			if err != nil {
				return "", err
			}
			resolved, e := filepath.EvalSymlinks(p)
			if e != nil || !within(root, resolved) {
				return "", errors.New("component symlink escapes object")
			}
			size = int64(len(link))
		} else {
			if immutable && st.Mode().Perm()&0222 != 0 && (runtime.GOOS != "windows" || st.Mode().IsRegular()) {
				return "", errors.New("component object is mutable")
			}
			if runtime.GOOS != "windows" && st.Mode().Perm()&0077 != 0 {
				return "", errors.New("component object is not private")
			}
			if st.Mode().IsRegular() {
				kind = "f"
				size = st.Size()
				if st.Mode().Perm()&0111 != 0 {
					executable = 1
				}
			} else if !st.IsDir() {
				return "", errors.New("special file in component object")
			}
		}
		total += size
		if size < 0 || total > componentExpandedLimit {
			return "", errors.New("component expanded size limit exceeded")
		}
		fmt.Fprintf(h, "%s %d:%s %d %d\n", kind, len(rel), rel, executable, size)
		if kind == "l" {
			_, err = io.WriteString(h, link)
		} else if kind == "f" {
			f, e := os.Open(p)
			if e != nil {
				return "", e
			}
			_, err = io.Copy(h, io.LimitReader(f, size+1))
			ce := f.Close()
			if err == nil {
				err = ce
			}
		}
		if err != nil {
			return "", err
		}
		_, _ = h.Write([]byte{'\n'})
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
func sealComponentTree(root string) error {
	var directories []string
	err := filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			directories = append(directories, p)
			return nil
		}
		if d.Type()&os.ModeSymlink != 0 {
			return nil
		}
		st, err := d.Info()
		if err != nil {
			return err
		}
		mode := os.FileMode(0400)
		if st.Mode().Perm()&0111 != 0 {
			mode = 0500
		}
		return os.Chmod(p, mode)
	})
	if err != nil {
		return err
	}
	for i := len(directories) - 1; i >= 0; i-- {
		if err = syncDirectory(directories[i]); err != nil {
			return err
		}
		if err = os.Chmod(directories[i], 0500); err != nil {
			return err
		}
	}
	return nil
}
func extractComponent(archive, format, root string) error {
	if err := os.Mkdir(root, 0700); err != nil {
		return err
	}
	seen := make(map[string]bool)
	links := make(map[string]string)
	var total int64
	add := func(name string, mode os.FileMode, size int64, source io.Reader, link string) error {
		name = strings.TrimSuffix(name, "/")
		if !relativeComponentPath(name) || seen[name] || len(seen) >= componentEntryLimit || size < 0 {
			return errors.New("unsafe or duplicate archive entry")
		}
		seen[name] = true
		total += size
		if total > componentExpandedLimit {
			return errors.New("component expansion exceeds limit")
		}
		p := filepath.Join(root, filepath.FromSlash(name))
		if err := os.MkdirAll(filepath.Dir(p), 0700); err != nil {
			return err
		}
		if err := cleanPath(filepath.Dir(p)); err != nil {
			return err
		}
		if mode&os.ModeSymlink != 0 {
			if len(link) == 0 || len(link) > 4096 || filepath.IsAbs(link) || strings.ContainsAny(link, "\\:\x00\r\n") || !within(root, filepath.Clean(filepath.Join(filepath.Dir(p), filepath.FromSlash(link)))) {
				return errors.New("unsafe component archive link")
			}
			links[p] = link
			return nil
		}
		if mode.IsDir() {
			return os.MkdirAll(p, 0700)
		}
		if !mode.IsRegular() {
			return errors.New("special file in component archive")
		}
		perm := os.FileMode(0600)
		if mode.Perm()&0111 != 0 {
			perm = 0700
		}
		out, err := os.OpenFile(p, os.O_WRONLY|os.O_CREATE|os.O_EXCL, perm)
		if err != nil {
			return err
		}
		n, err := io.Copy(out, io.LimitReader(source, size+1))
		if err == nil && n != size {
			err = errors.New("archive entry size mismatch")
		}
		if err == nil {
			err = out.Sync()
		}
		ce := out.Close()
		if err == nil {
			err = ce
		}
		return err
	}
	switch format {
	case "zip":
		z, err := zip.OpenReader(archive)
		if err != nil {
			return err
		}
		defer z.Close()
		for _, f := range z.File {
			if f.UncompressedSize64 > uint64(componentExpandedLimit) {
				return errors.New("component ZIP entry exceeds limit")
			}
			reader, err := f.Open()
			if err != nil {
				return err
			}
			link := ""
			if f.Mode()&os.ModeSymlink != 0 {
				data, e := io.ReadAll(io.LimitReader(reader, 4097))
				if e != nil {
					reader.Close()
					return e
				}
				link = string(data)
			}
			err = add(f.Name, f.Mode(), int64(f.UncompressedSize64), reader, link)
			ce := reader.Close()
			if err != nil {
				return err
			}
			if ce != nil {
				return ce
			}
		}
	case "tar.gz":
		f, err := os.Open(archive)
		if err != nil {
			return err
		}
		defer f.Close()
		z, err := gzip.NewReader(f)
		if err != nil {
			return err
		}
		defer z.Close()
		tr := tar.NewReader(z)
		for {
			header, e := tr.Next()
			if e == io.EOF {
				break
			}
			if e != nil {
				return e
			}
			if header.Typeflag != tar.TypeReg && header.Typeflag != tar.TypeRegA && header.Typeflag != tar.TypeDir && header.Typeflag != tar.TypeSymlink {
				return errors.New("unsupported component tar entry")
			}
			if err = add(header.Name, header.FileInfo().Mode(), header.Size, tr, header.Linkname); err != nil {
				return err
			}
		}
		if _, err = io.Copy(io.Discard, io.LimitReader(z, 1024)); err != nil {
			return err
		}
	default:
		return errors.New("unsupported component archive format")
	}
	for p, target := range links {
		if err := os.Symlink(filepath.FromSlash(target), p); err != nil {
			return err
		}
	}
	_, err := componentTreeDigest(root, false)
	return err
}
func (o *componentOperation) stageObjects() error {
	r := o.request
	for _, name := range []string{"objects", "archives", "receipts"} {
		p := filepath.Join(r.StoreDirectory, name)
		if err := os.Mkdir(p, 0700); err != nil && !errors.Is(err, os.ErrExist) {
			return err
		}
		if err := privatePath(p, true); err != nil {
			return err
		}
	}
	for _, id := range componentIDs {
		c := r.TargetComponents[id]
		archive, changed := r.Staged[id]
		if !changed {
			if err := verifyObject(r, c); err != nil {
				return fmt.Errorf("reused %s: %w", id, err)
			}
			continue
		}
		if err := verifyComponentArchive(archive, c.Component.Artifact); err != nil {
			return err
		}
		root := componentRoot(r, c)
		if _, err := os.Lstat(root); err == nil {
			if err = verifyObject(r, c); err != nil {
				return err
			}
			continue
		} else if !errors.Is(err, os.ErrNotExist) {
			return err
		}
		// Every temporary path belongs only to this operation. Interrupted payloads
		// are retained, never mistaken for completed immutable objects or deleted.
		temp := filepath.Join(r.StoreDirectory, "objects", ".stage-"+r.OperationID+"-"+id)
		retained := filepath.Join(r.StoreDirectory, "archives", c.Component.Artifact.SHA256)
		if _, err := os.Lstat(retained); errors.Is(err, os.ErrNotExist) {
			if err = copyFile(archive, retained, 0400); err != nil {
				return err
			}
		} else if err != nil {
			return err
		}
		if err := verifyComponentArchive(retained, c.Component.Artifact); err != nil {
			return err
		}
		if err := extractComponent(retained, c.Component.Artifact.Format, temp); err != nil {
			return err
		}
		image := filepath.Join(temp, filepath.FromSlash(c.Component.Executable))
		if actual, err := componentFileHash(image); err != nil || actual != c.Component.ExecutableSHA256 {
			return errors.New("staged component executable digest mismatch")
		}
		if err := validateComponentDescriptor(temp, c.Component, true); err != nil {
			return err
		}
		if err := sealComponentTree(temp); err != nil {
			return err
		}
		if runtime.GOOS == "darwin" || runtime.GOOS == "windows" {
			if err := validateComponentSignature(r, id, temp); err != nil {
				return err
			}
		}
		tree, err := componentTreeDigest(temp, true)
		if err != nil {
			return err
		}
		receipt := componentReceipt{SchemaVersion: 1, Components: make(map[string]componentRef), Packages: []componentPackage{}, TreeSHA512: tree}
		for member, selected := range r.TargetComponents {
			if selected.Component.Artifact.SHA256 == c.Component.Artifact.SHA256 {
				if !reflect.DeepEqual(selected.Component.Artifact, c.Component.Artifact) {
					return errors.New("shared object artifact metadata differs")
				}
				if imageHash, e := componentFileHash(filepath.Join(temp, filepath.FromSlash(selected.Component.Executable))); e != nil || imageHash != selected.Component.ExecutableSHA256 {
					return errors.New("cohort image digest mismatch")
				}
				if err = validateComponentDescriptor(temp, selected.Component, true); err != nil {
					return err
				}
				receipt.Components[member] = selected.Component
			}
		}
		inventory, err := readComponentInventory(r.InventoryPath)
		if err != nil {
			return err
		}
		for _, pkg := range inventory.Targets[r.Target].Packages {
			if reflect.DeepEqual(pkg.Artifact, c.Component.Artifact) {
				receipt.Packages = append(receipt.Packages, pkg)
			}
		}
		if err = atomicJSON(filepath.Join(r.StoreDirectory, "receipts", c.Component.Artifact.SHA256+".json"), receipt); err != nil {
			return err
		}
		// Publish the fully verified payload last: an interrupted receipt write
		// cannot leave a visible object that lacks its authenticated descriptor.
		if err = os.Rename(temp, root); err != nil {
			return err
		}
		if err = syncDirectory(filepath.Dir(root)); err != nil {
			return err
		}
	}
	return nil
}

// pruneComponentStore deletes every digest-named object, archive and receipt
// that no kept set references. Interrupted `.stage-`/`.import-` payloads belong
// to their operations and stay; `.pruning-` trees are earlier detached objects.
// It continues past failures and returns them joined.
func pruneComponentStore(r componentRequest, keep ...componentSet) error {
	referenced := make(map[string]bool)
	for _, set := range keep {
		for _, c := range set {
			referenced[c.Component.Artifact.SHA256] = true
		}
	}
	objects := filepath.Join(r.StoreDirectory, "objects")
	superseded := make(map[string]bool)
	var detached []string
	var failures []error
	for _, kind := range []string{"objects", "archives", "receipts"} {
		entries, err := os.ReadDir(filepath.Join(r.StoreDirectory, kind))
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			failures = append(failures, err)
			continue
		}
		for _, entry := range entries {
			name := entry.Name()
			if kind == "receipts" {
				var ok bool
				if name, ok = strings.CutSuffix(name, ".json"); !ok {
					continue
				}
			}
			if kind == "objects" && strings.HasPrefix(name, ".pruning-") {
				detached = append(detached, filepath.Join(objects, name))
			} else if sha256Pattern.MatchString(name) && !referenced[name] {
				superseded[name] = true
			}
		}
	}
	for _, p := range detached {
		if err := removeComponentTree(p); err != nil {
			failures = append(failures, err)
		}
	}
	for digest := range superseded {
		// Detach first: an object that cannot leave its digest name keeps its
		// archive and receipt, so it is never left partially verifiable.
		object, temporary := filepath.Join(objects, digest), filepath.Join(objects, ".pruning-"+digest)
		if err := os.Rename(object, temporary); err != nil && !errors.Is(err, os.ErrNotExist) {
			failures = append(failures, err)
			continue
		}
		for _, p := range []string{filepath.Join(r.StoreDirectory, "receipts", digest+".json"), filepath.Join(r.StoreDirectory, "archives", digest)} {
			if err := os.Remove(p); err != nil && !errors.Is(err, os.ErrNotExist) {
				failures = append(failures, err)
			}
		}
		if err := removeComponentTree(temporary); err != nil {
			failures = append(failures, err)
		}
	}
	return errors.Join(failures...)
}

// removeComponentTree makes a sealed (read-only) tree writable, then removes it.
func removeComponentTree(root string) error {
	err := filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if errors.Is(err, os.ErrNotExist) && p == root {
			return filepath.SkipAll
		}
		if err != nil || d.Type()&os.ModeSymlink != 0 {
			return err
		}
		mode := os.FileMode(0600)
		if d.IsDir() {
			mode = 0700
		}
		return os.Chmod(p, mode)
	})
	if err != nil {
		return err
	}
	return removeTree(root)
}
func validateComponentSignature(r componentRequest, id, stagedRoot string) error {
	c := r.TargetComponents[id]
	newImage := filepath.Join(stagedRoot, filepath.FromSlash(c.Component.Executable))
	oldImage := componentImage(r, r.Current[id])
	if runtime.GOOS == "windows" {
		return verifyPublisherContinuity(oldImage, newImage, "component", "installed component")
	}
	if id == "app" {
		bundle := func(p string) string {
			for p != filepath.Dir(p) {
				if strings.HasSuffix(p, ".app") {
					return p
				}
				p = filepath.Dir(p)
			}
			return ""
		}
		oldBundle, newBundle := bundle(oldImage), bundle(newImage)
		if oldBundle == "" || newBundle == "" {
			return errors.New("macOS app is not an intact bundle")
		}
		return validateMacApp(oldBundle, newBundle, c.Component.Identity.Version)
	}
	// A separately selected native image must remain signed by the same team.
	if err := exec.Command("/usr/bin/codesign", "--verify", "--strict", newImage).Run(); err != nil {
		return err
	}
	team := func(p string) (string, error) {
		b, e := exec.Command("/usr/bin/codesign", "-d", "--verbose=4", p).CombinedOutput()
		if e != nil {
			return "", e
		}
		for _, line := range strings.Split(string(b), "\n") {
			if strings.HasPrefix(line, "TeamIdentifier=") {
				return strings.TrimPrefix(line, "TeamIdentifier="), nil
			}
		}
		return "", errors.New("missing component signing team")
	}
	oldTeam, err := team(oldImage)
	if err != nil {
		return err
	}
	newTeam, err := team(newImage)
	if err != nil {
		return err
	}
	if oldTeam == "" || oldTeam == "not set" || oldTeam != newTeam {
		return errors.New("component publisher changed")
	}
	return nil
}

type boundedComponentOutput struct{ bytes.Buffer }

func (b *boundedComponentOutput) Write(p []byte) (int, error) {
	if len(p) > maxJSON-b.Len() {
		return 0, errors.New("helper identity output exceeds limit")
	}
	return b.Buffer.Write(p)
}
func queryComponentHelper(r componentRequest, c installedComponent) error {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, componentImage(r, c), "--identity")
	cmd.Env = scrubUpdateEnvironment(os.Environ(), 0)
	var out boundedComponentOutput
	cmd.Stdout = &out
	cmd.Stderr = io.Discard
	hideWindow(cmd)
	if err := cmd.Run(); err != nil {
		return err
	}
	var actual struct {
		SchemaVersion int               `json:"schemaVersion"`
		ID            string            `json:"id"`
		Identity      componentIdentity `json:"identity"`
	}
	if err := unambiguousJSON(out.Bytes()); err != nil {
		return err
	}
	d := json.NewDecoder(bytes.NewReader(out.Bytes()))
	d.DisallowUnknownFields()
	if err := d.Decode(&actual); err != nil {
		return err
	}
	if d.Decode(new(any)) != io.EOF || actual.SchemaVersion != 1 || actual.ID != "update-helper" || actual.Identity != c.Component.Identity {
		return errors.New("selected helper identity/ABI mismatch")
	}
	return nil
}
