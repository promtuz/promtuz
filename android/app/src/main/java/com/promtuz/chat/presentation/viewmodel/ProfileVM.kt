package com.promtuz.chat.presentation.viewmodel

import android.graphics.Bitmap
import androidx.compose.ui.graphics.ImageBitmap
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.utils.media.PhotoCrop
import com.promtuz.chat.utils.media.decodeAvatar
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

data class OwnProfile(val name: String = "", val picture: ImageBitmap? = null)

sealed interface ProfileWork {
    data object Idle : ProfileWork
    data object Busy : ProfileWork
    data class Failed(val reason: String) : ProfileWork
}

/** Shared by Profile and its editor so leaving the editor cannot interrupt an accepted save. */
class ProfileVM : ViewModel() {
    private val _profile = MutableStateFlow(OwnProfile())
    val profile = _profile.asStateFlow()
    private val _work = MutableStateFlow<ProfileWork>(ProfileWork.Idle)
    val work = _work.asStateFlow()
    private var refreshJob: Job? = null
    private val _savedPhoto = MutableStateFlow<String?>(null)
    val savedPhoto = _savedPhoto.asStateFlow()

    fun refresh() {
        if (_work.value == ProfileWork.Busy) return
        refreshJob?.cancel()
        refreshJob = viewModelScope.launch {
            try {
                readProfile()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.tag("Profile").w(e, "Could not read profile")
                _work.value = ProfileWork.Failed("Couldn't load your profile")
            }
        }
    }

    private suspend fun readProfile() {
        val name = CoreBridge.profileName()
        val picture = CoreBridge.profilePicture()?.let { decodeAvatar(it) }
        _profile.value = OwnProfile(name, picture)
    }

    fun savePicture(source: Bitmap, crop: PhotoCrop, requestId: String) = change({ _savedPhoto.value = requestId }) {
        val rgba = withContext(Dispatchers.Default) {
            val bitmap = crop.render(source)
            try { bitmap.toRgba() } finally { bitmap.recycle() }
        }
        CoreBridge.setProfilePicture(rgba, PhotoCrop.OUTPUT_EDGE, PhotoCrop.OUTPUT_EDGE)
    }

    fun removePicture() = change { CoreBridge.clearProfilePicture() }

    private fun change(onSaved: () -> Unit = {}, write: suspend () -> Unit) {
        if (_work.value == ProfileWork.Busy) return
        _work.value = ProfileWork.Busy // Guard rapid taps before launching the coroutine.
        refreshJob?.cancel()
        viewModelScope.launch {
            try {
                write()
                readProfile()
                _work.value = ProfileWork.Idle
                onSaved()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.tag("Profile").w(e, "Picture change failed")
                _work.value = ProfileWork.Failed("Couldn't update your photo. Try again.")
            }
        }
    }

    fun dismissError() {
        if (_work.value is ProfileWork.Failed) _work.value = ProfileWork.Idle
    }
}
