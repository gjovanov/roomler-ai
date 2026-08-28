// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { ref, onBeforeUnmount } from 'vue'

export function usePictureInPicture() {
  const isPiPActive = ref(false)
  const isSupported = ref(!!document.pictureInPictureEnabled)

  function onLeavePiP() {
    isPiPActive.value = false
  }

  document.addEventListener('leavepictureinpicture', onLeavePiP)

  async function requestPiP(videoElement: HTMLVideoElement) {
    if (!document.pictureInPictureEnabled) return
    try {
      await videoElement.requestPictureInPicture()
      isPiPActive.value = true
    } catch (err) {
      console.warn('[PiP] requestPictureInPicture failed:', err)
    }
  }

  async function exitPiP() {
    if (!document.pictureInPictureElement) return
    try {
      await document.exitPictureInPicture()
      isPiPActive.value = false
    } catch (err) {
      console.warn('[PiP] exitPictureInPicture failed:', err)
    }
  }

  function cleanup() {
    document.removeEventListener('leavepictureinpicture', onLeavePiP)
  }

  onBeforeUnmount(cleanup)

  return {
    isPiPActive,
    isSupported,
    requestPiP,
    exitPiP,
  }
}
