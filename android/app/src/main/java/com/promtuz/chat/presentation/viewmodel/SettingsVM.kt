package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import android.net.Uri
import androidx.compose.ui.graphics.ImageBitmap
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.utils.media.decodeAvatar
import com.promtuz.chat.utils.media.decodeDownscaled
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import timber.log.Timber

/** Our own profile as the settings header shows it: name and picture. */
data class OwnProfile(val name: String = "", val picture: ImageBitmap? = null)

/** What the picture is doing right now, so the header can hold still. */
sealed interface ProfileWork {
    data object Idle : ProfileWork
    data object Busy : ProfileWork
    data class Failed(val reason: String) : ProfileWork
}

class SettingsVM(
    private val application: Application
) : ViewModel() {
    private val context: Context get() = application.applicationContext

    private val _profile = MutableStateFlow(OwnProfile())
    val profile: StateFlow<OwnProfile> = _profile.asStateFlow()

    private val _work = MutableStateFlow<ProfileWork>(ProfileWork.Idle)
    val work: StateFlow<ProfileWork> = _work.asStateFlow()

    init {
        refresh()
    }

    /**
     * Re-read the profile from core. The header shows what core stored, not
     * what was picked: those are the exact bytes every contact will see.
     */
    private fun refresh() = viewModelScope.launch {
        val name = runCatching { CoreBridge.profileName() }.getOrDefault("")
        val picture = runCatching { CoreBridge.profilePicture() }.getOrNull()
            ?.let { decodeAvatar(it) }
        _profile.value = OwnProfile(name, picture)
    }

    /** Decode the pick to a bitmap core can crop from, and hand core its RGBA. */
    fun setPicture(uri: Uri) = viewModelScope.launch {
        _work.value = ProfileWork.Busy
        finish(runCatching {
            val bitmap = decodeDownscaled(context, uri, SOURCE_EDGE)
                ?: error("Couldn't read that image")
            CoreBridge.setProfilePicture(bitmap.toRgba(), bitmap.width, bitmap.height)
        })
    }

    fun removePicture() = viewModelScope.launch {
        _work.value = ProfileWork.Busy
        finish(runCatching { CoreBridge.clearProfilePicture() })
    }

    private suspend fun finish(result: Result<Unit>) {
        result.onFailure { Timber.tag("Profile").w(it, "picture change failed") }
        _work.value = result.fold(
            { ProfileWork.Idle },
            { ProfileWork.Failed(it.message ?: "Couldn't update your picture") },
        )
        // Core notifies the shared avatar cache after storing the change.
        refresh().join()
    }

    fun dismissError() {
        if (_work.value is ProfileWork.Failed) _work.value = ProfileWork.Idle
    }

    private companion object {
        /**
         * Longest edge to decode the pick at: more than core keeps, so its crop
         * has something to resample from, and small enough to hand over as RGBA.
         */
        const val SOURCE_EDGE = 1024
    }
}
