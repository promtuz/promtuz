package com.promtuz.chat.presentation.viewmodel

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.promtuz.chat.utils.media.PhotoCrop
import com.promtuz.chat.utils.media.AvatarPicture
import com.promtuz.chat.utils.media.AvatarSource
import com.promtuz.chat.utils.media.ImagePreparationException
import com.promtuz.chat.utils.media.prepareAvatar
import com.promtuz.chat.utils.media.toRgba
import com.promtuz.chat.utils.extensions.fromHex
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

data class OwnProfile(val name: String = "", val picture: AvatarPicture? = null, val bio: String = "", val identity: String = "")

sealed interface ProfileWork {
    data object Idle : ProfileWork
    data object Busy : ProfileWork
    data class Failed(val reason: String) : ProfileWork
}

/** Shared by Profile and its editor so leaving the editor cannot interrupt an accepted save. */
class ProfileVM(private val group: String? = null) : ViewModel() {
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
        val name = if (group == null) CoreBridge.profileName() else CoreBridge.conversation(group.fromHex())?.displayName.orEmpty()
        val picture = (if (group == null) CoreBridge.profilePicture() else CoreBridge.groupPicture(group.fromHex()))?.let { prepareAvatar(it) }
        _profile.value = OwnProfile(name, picture, if (group == null) CoreBridge.profileBio() else "", if (group == null) withContext(Dispatchers.IO) { uniffi.core.profileIdentity().toHex() } else group)
    }

    fun savePicture(source: AvatarSource, crop: PhotoCrop, requestId: String) = change({ _savedPhoto.value = requestId }) {
        when (source) {
            is AvatarSource.Still -> {
                val rgba = withContext(Dispatchers.Default) {
                    val bitmap = crop.render(source.bitmap)
                    try { bitmap.toRgba() } finally { bitmap.recycle() }
                }
                if (group == null) CoreBridge.setProfilePicture(rgba, PhotoCrop.OUTPUT_EDGE, PhotoCrop.OUTPUT_EDGE)
                else CoreBridge.setGroupPicture(group.fromHex(), rgba, PhotoCrop.OUTPUT_EDGE, PhotoCrop.OUTPUT_EDGE)
            }
            is AvatarSource.Encoded -> {
                val bytes = if (source.canCrop) CoreBridge.prepareAvatarImage(source.original,
                    uniffi.core.AvatarCropRecord(crop.centerX.toDouble(), crop.centerY.toDouble(), crop.zoom.toDouble()),
                ).bytes else source.picture.encoded
                if (group == null) CoreBridge.setProfilePictureEncoded(bytes)
                else CoreBridge.setGroupPictureEncoded(group.fromHex(), bytes)
            }
        }
    }

    fun removePicture() = change {
        if (group == null) CoreBridge.clearProfilePicture() else CoreBridge.setGroupPicture(group.fromHex(), null)
    }

    fun saveDetails(name: String, bio: String, onSaved: () -> Unit) = change(onSaved) {
        CoreBridge.setProfileDetails(name, bio)
    }

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
                val reason = when (e) {
                    is uniffi.core.CoreException.Refused -> e.msg
                    is ImagePreparationException -> e.message
                    else -> null
                }
                _work.value = ProfileWork.Failed(reason ?: "Couldn't save your changes. Try again.")
            }
        }
    }

    fun dismissError() {
        if (_work.value is ProfileWork.Failed) _work.value = ProfileWork.Idle
    }
}
