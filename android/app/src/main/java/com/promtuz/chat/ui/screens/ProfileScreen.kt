package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.AppAlertDialog
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import androidx.compose.runtime.saveable.rememberSaveable
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import org.koin.compose.koinInject
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.pictureItem

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileScreen(viewModel: ProfileVM, onChoosePhoto: (Uri) -> Unit) {
    val app = koinInject<AppVM>()
    var editing by rememberSaveable { mutableStateOf(false) }
    var draftName by rememberSaveable { mutableStateOf("") }
    var draftBio by rememberSaveable { mutableStateOf("") }
    val profile by viewModel.profile.collectAsState()
    val work by viewModel.work.collectAsState()
    val busy = work == ProfileWork.Busy
    LaunchedEffect(Unit) { viewModel.refresh() }
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null) {
            viewModel.dismissError()
            onChoosePhoto(uri)
        }
    }
    val choose = {
        picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
    }
    val editLabel = stringResource(R.string.profile_photo_edit)
    val chooseLabel = stringResource(R.string.profile_photo_choose)
    val removeLabel = stringResource(R.string.profile_photo_remove)

    SimpleScreen({ Text("Profile") }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = PaddingValues(
                start = 18.dp, end = 18.dp,
                top = padding.calculateTopPadding() + 20.dp,
                bottom = padding.calculateBottomPadding() + 24.dp,
            ),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            item {
                val tile: @Composable (onClick: (() -> Unit)?) -> Unit = { onClick ->
                    Box(Modifier.semantics { contentDescription = editLabel }) {
                        Avatar(profile.name, size = 96.dp, identityKey = profile.identity, image = profile.picture, onClick = onClick, originKey = "profile-photo")
                        Box(
                            Modifier.align(Alignment.BottomEnd).size(30.dp).clip(CircleShape)
                                .background(MaterialTheme.colorScheme.primary),
                            contentAlignment = Alignment.Center,
                        ) {
                            if (busy) CircularProgressIndicator(
                                Modifier.size(16.dp), color = MaterialTheme.colorScheme.onPrimary,
                                strokeWidth = 2.dp,
                            ) else Icon(
                                painterResource(R.drawable.oi_camera), null, Modifier.size(16.dp),
                                tint = MaterialTheme.colorScheme.onPrimary,
                            )
                        }
                    }
                }
                val picture = profile.picture
                if (picture != null && !busy) tile {
                    MediaViewer.open(listOf(pictureItem("profile-photo", picture, profile.name, actions = listOf(
                        listOf(MenuAction(chooseLabel, R.drawable.oi_image) { MediaViewer.close(); choose() }),
                        listOf(MenuAction(removeLabel, R.drawable.oi_trash, destructive = true) {
                            MediaViewer.close(); viewModel.removePicture()
                        }),
                    ))))
                } else tile(if (busy) null else choose)
            }
            item { Text(profile.name, style = MaterialTheme.typography.titleLargeEmphasized) }
            item {
                if (profile.bio.isNotBlank()) Text(profile.bio, style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            item {
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    GroupedActionRow("Edit profile", 0, 2, enabled = !busy,
                        supportingText = "Name and bio", onClick = {
                            draftName = profile.name; draftBio = profile.bio; editing = true
                        }) { DrawableIcon(R.drawable.i_user, size = 26.dp) }
                    GroupedActionRow("Share my identity", 1, 2,
                        supportingText = "Your QR code and invite link",
                        onClick = { app.navigator.push(Routes.ShareIdentity) }) { DrawableIcon(R.drawable.oi_qr_code, size = 26.dp) }
                }
            }
            item {
                Text("ACCOUNT", Modifier.fillMaxWidth().padding(top = 18.dp),
                    style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            item {
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    GroupedActionRow("Identity & keys", 0, 2,
                        supportingText = "Public identity and recovery phrase",
                        onClick = { app.navigator.push(Routes.IdentityKeys) }) { DrawableIcon(R.drawable.i_key, size = 26.dp) }
                    GroupedActionRow("Backup & restore", 1, 2,
                        supportingText = "Keep a copy of your chats",
                        onClick = { app.navigator.push(Routes.BackupRestore) }) { DrawableIcon(R.drawable.i_chat_backup, size = 26.dp) }
                }
            }
            (work as? ProfileWork.Failed)?.let { failure ->
                item { Text(failure.reason, color = MaterialTheme.colorScheme.error) }
            }
        }
    }
    if (editing) AppAlertDialog(
        onDismissRequest = { if (!busy) editing = false },
        title = { Text("Edit profile") },
        text = { Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedTextField(draftName, { if (it.length <= 32) draftName = it },
                label = { Text("Name") }, singleLine = true, enabled = !busy)
            OutlinedTextField(draftBio, { if (it.length <= 160) draftBio = it },
                label = { Text("Bio") }, minLines = 2, maxLines = 4, enabled = !busy,
                supportingText = { Text("${draftBio.length}/160") })
            if (work is ProfileWork.Failed) Text((work as ProfileWork.Failed).reason, color = MaterialTheme.colorScheme.error)
        } },
        confirmButton = { TextButton(enabled = !busy && draftName.isNotBlank(), onClick = {
            viewModel.saveDetails(draftName.trim(), draftBio.trim()) { editing = false }
        }) { Text(if (busy) "Saving…" else "Save") } },
        dismissButton = { TextButton(enabled = !busy, onClick = { editing = false }) { Text("Cancel") } },
    )
}
