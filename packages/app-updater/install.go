package main

import (
	"errors"
	"os/exec"
	"path/filepath"
	"strings"
)

func validateMacApp(old, new, version string) error {
	plist := filepath.Join(new, "Contents", "Info.plist")
	id, err := commandOutput("/usr/libexec/PlistBuddy", "-c", "Print :CFBundleIdentifier", plist)
	if err != nil {
		return err
	}
	actualVersion, err := commandOutput("/usr/libexec/PlistBuddy", "-c", "Print :CFBundleShortVersionString", plist)
	if err != nil {
		return err
	}
	if id != "sh.paseo.sessions.desktop" || actualVersion != version {
		return errors.New("app bundle identity/version mismatch")
	}
	if err = exec.Command("/usr/bin/codesign", "--verify", "--deep", "--strict", new).Run(); err != nil {
		return err
	}
	signature := func(path string) (string, error) {
		output, err := exec.Command("/usr/bin/codesign", "-d", "--verbose=4", path).CombinedOutput()
		return string(output), err
	}
	before, err := signature(old)
	if err != nil {
		return err
	}
	after, err := signature(new)
	if err != nil {
		return err
	}
	team := func(text string) string {
		for _, line := range strings.Split(text, "\n") {
			if value, ok := strings.CutPrefix(line, "TeamIdentifier="); ok && value != "not set" {
				return value
			}
		}
		return ""
	}
	if previous := team(before); previous != "" {
		if team(after) != previous {
			return errors.New("macOS publisher changed")
		}
		return exec.Command("/usr/sbin/spctl", "--assess", "--type", "execute", new).Run()
	}
	if !strings.Contains(before, "Signature=adhoc") || !strings.Contains(after, "Signature=adhoc") {
		return errors.New("unverifiable macOS publisher")
	}
	return nil
}
