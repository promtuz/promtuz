package com.promtuz.chat.security

import com.promtuz.chat.ui.camera.CameraLauncher
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.PrivateVideoPlayers
import com.promtuz.chat.utils.media.AvatarImages
import com.promtuz.chat.utils.media.PrivateMediaAccess
import com.promtuz.chat.utils.media.StickerImages
import com.promtuz.chat.utils.media.VoicePlayer
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow

/** Process-local capability. Never serialized into navigation or saved instance state. */
object PrivateChatSession {
    private val policy = PrivateSessionPolicy()
    private val handler = Handler(Looper.getMainLooper())
    private val _active = MutableStateFlow<String?>(null)
    val active = _active.asStateFlow()
    private var inputOwners = 0
    private val _secure = MutableStateFlow(false)
    val secure = _secure.asStateFlow()
    fun protectInput(): AutoCloseable {
        inputOwners++
        _secure.value = true
        var open = true
        return AutoCloseable {
            if (!open) return@AutoCloseable
            open = false
            inputOwners--
            _secure.value = inputOwners > 0 || _active.value != null
        }
    }
    private val _exits = MutableStateFlow(0L)
    val exits = _exits.asStateFlow()
    private val _revision = MutableStateFlow(0L)
    val revision = _revision.asStateFlow()
    @Volatile var sessionId: Long? = null
        private set
    /** Core owns the only counter; any lock or cancel moves it and refuses older unlocks. */
    val generation: Long get() = uniffi.core.privateGeneration().toLong()
    @Volatile var foreground: Boolean = false
        private set
    private val expiry = Runnable { if (policy.expire(SystemClock.elapsedRealtime())) lock() }

    fun arm(conversationHex: String): Boolean {
        if (!foreground) return false
        if (_active.value != conversationHex || sessionId == null) sessionId = generation
        policy.arm()
        _active.value = conversationHex
        _revision.value++
        _secure.value = true
        return true
    }
    fun clear() {
        val hadPrivateContent = _active.value != null
        sessionId = null
        policy.clear()
        handler.removeCallbacks(expiry)
        // Lock even when no result has appeared: an in-flight unlock may still be running.
        uniffi.core.lockPrivateConversations()
        _active.value = null
        _revision.value++
        PrivateMediaAccess.clear()
        CameraLauncher.close()
        if (hadPrivateContent) {
            PrivateVideoPlayers.clear()
            MediaViewer.close()
            VoicePlayer.stop()
            com.promtuz.chat.utils.media.clearDecodedImageCache()
            AvatarImages.invalidateAll()
            StickerImages.clear()
        }
        _secure.value = inputOwners > 0
    }
    fun lock() {
        val wasActive = _active.value != null
        clear()
        if (wasActive) _exits.value++
    }
    fun beginHandoff() {
        if (_active.value == null) return
        policy.handoff(SystemClock.elapsedRealtime())
        // Policy retains its original deadline even if a second launch occurs.
        handler.removeCallbacks(expiry)
        handler.postDelayed(expiry, policy.remaining(SystemClock.elapsedRealtime()))
    }
    fun resumed(screenInteractive: Boolean) {
        foreground = true
        if (!screenInteractive || policy.resume(SystemClock.elapsedRealtime())) lock()
    }
    fun paused() {
        foreground = false
        uniffi.core.cancelPrivateUnlocks()
        if (policy.depart(SystemClock.elapsedRealtime())) lock()
        else if (_active.value == null) clear()
    }
    fun screenOff() {
        foreground = false
        uniffi.core.cancelPrivateUnlocks()
        if (policy.screenOff()) lock() else clear()
    }
}
