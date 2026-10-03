package com.promtuz.chat.ui.screens

import android.net.Uri
import androidx.compose.foundation.Image
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.drawscope.withTransform
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import com.promtuz.chat.ui.components.AVATAR_RADIUS_RATIO
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.utils.media.PhotoCrop
import com.promtuz.chat.utils.media.AvatarSource
import com.promtuz.chat.utils.media.EncodedImageContent
import com.promtuz.chat.utils.media.ImagePreparationException
import com.promtuz.chat.utils.media.pickAvatarSource
import kotlinx.coroutines.CancellationException
import java.util.UUID

@Composable
fun ProfilePhotoScreen(uri: String, viewModel: ProfileVM, onSaved: () -> Unit) {
    val context = LocalContext.current
    var source by remember(uri) { mutableStateOf<AvatarSource?>(null) }
    var loading by remember(uri) { mutableStateOf(true) }
    var loadError by remember(uri) { mutableStateOf<String?>(null) }
    var centerX by rememberSaveable(uri) { mutableFloatStateOf(.5f) }
    var centerY by rememberSaveable(uri) { mutableFloatStateOf(.5f) }
    var zoom by rememberSaveable(uri) { mutableFloatStateOf(1f) }
    val crop = PhotoCrop(centerX, centerY, zoom)
    val work by viewModel.work.collectAsState()
    val busy = work == ProfileWork.Busy
    val requestId = rememberSaveable(uri) { UUID.randomUUID().toString() }
    val savedPhoto by viewModel.savedPhoto.collectAsState()
    val currentOnSaved by rememberUpdatedState(onSaved)
    // Survives rotation during Save, without delivering completion to a later editor.
    LaunchedEffect(savedPhoto) {
        if (savedPhoto == requestId) currentOnSaved()
    }
    LaunchedEffect(uri) {
        source = try {
            pickAvatarSource(context, Uri.parse(uri))
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            loadError = when (e) {
                is uniffi.core.CoreException.Refused -> e.msg
                is ImagePreparationException -> e.message
                else -> null
            }
            null
        }
        loading = false
    }

    SimpleScreen(
        title = { Text("Profile photo") },
        actions = {
            TextButton(
                enabled = source != null && !busy,
                onClick = { source?.let { photo ->
                    viewModel.savePicture(photo, crop, requestId)
                } },
            ) {
                if (busy) CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                else Text("Save")
            }
        },
    ) { padding ->
        BoxWithConstraints(Modifier.fillMaxSize().padding(padding), contentAlignment = Alignment.Center) {
            val edge = minOf(maxWidth - 32.dp, maxHeight - 100.dp).coerceAtLeast(1.dp)
            Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(16.dp)) {
                val photo = source
                if (photo != null) {
                    val image = photo.poster
                    Box(
                        Modifier.size(edge).clip(RoundedCornerShape(edge / AVATAR_RADIUS_RATIO))
                            .semantics { contentDescription = "Profile photo preview" }
                            .pointerInput(photo, busy) {
                                if (!busy && photo.canCrop) detectTransformGestures { centroid, pan, scale, _ ->
                                    val before = PhotoCrop(centerX, centerY, zoom)
                                    val nextZoom = (zoom * scale).coerceIn(1f, 5f)
                                    val oldSide = before.side(photo.width, photo.height)
                                    val newSide = minOf(photo.width, photo.height) / nextZoom
                                    val focal = centroid - Offset(size.width / 2f, size.height / 2f)
                                    val shift = focal * ((oldSide - newSide) / size.width.toFloat()) -
                                        pan * (newSide / size.width.toFloat())
                                    val after = PhotoCrop(
                                        centerX + shift.x / photo.width,
                                        centerY + shift.y / photo.height, nextZoom,
                                    ).bounded(photo.width, photo.height)
                                    centerX = after.centerX; centerY = after.centerY; zoom = after.zoom
                                }
                            },
                    ) {
                        // Transform the shared renderer's draw, so every animation frame and
                        // HDR bitmap follows the same source-relative crop as the saved image.
                        val imageModifier = Modifier.fillMaxSize().drawWithContent {
                            val bounded = (if (photo.canCrop) crop else PhotoCrop()).bounded(photo.width, photo.height)
                            val side = bounded.side(photo.width, photo.height)
                            val sx = photo.width / side
                            val sy = photo.height / side
                            withTransform({
                                translate(size.width / 2f - bounded.centerX * size.width * sx,
                                    size.height / 2f - bounded.centerY * size.height * sy)
                                scale(sx, sy, pivot = Offset.Zero)
                            }) { this@drawWithContent.drawContent() }
                        }
                        when (photo) {
                            is AvatarSource.Still -> Image(image, null, imageModifier, contentScale = ContentScale.FillBounds)
                            is AvatarSource.Encoded -> EncodedImageContent(
                                photo.picture.prepared, null, imageModifier, contentScale = ContentScale.FillBounds,
                            )
                        }
                    }
                    Text(if (photo.canCrop) "Drag to move · Pinch to zoom" else "Original framing",
                        style = MaterialTheme.typography.bodyMedium)
                } else if (loading) CircularProgressIndicator()
                else Text(loadError ?: "Couldn't read that photo. Choose another one.", Modifier.padding(horizontal = 24.dp))
                (work as? ProfileWork.Failed)?.let {
                    Text(it.reason, Modifier.padding(horizontal = 24.dp), color = MaterialTheme.colorScheme.error)
                }
            }
        }
    }
}
