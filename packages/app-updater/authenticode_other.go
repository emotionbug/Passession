//go:build !windows

package main

import "errors"

func inspectAuthenticode(string) (authenticodeIdentity, error) {
	return authenticodeIdentity{}, errors.New("Authenticode verification requires Windows")
}
