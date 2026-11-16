package com.promtuz.chat.ui.screens

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.focus.FocusDirection
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.navigation.LocalNavForeground
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.media.MediaViewer
import com.promtuz.chat.ui.media.pictureItem
import com.promtuz.chat.ui.theme.Promtuz
import org.koin.compose.koinInject

private const val NameLimit = 32
private const val AboutLimit = 160

@Composable
fun ProfileScreen(viewModel: ProfileVM, onChoosePhoto: (Uri) -> Unit) {
    val app = koinInject<AppVM>()
    val c = Promtuz.colors
    val profile by viewModel.profile.collectAsState()
    val work by viewModel.work.collectAsState()
    val busy = work == ProfileWork.Busy
    val foreground = LocalNavForeground.current
    val focus = LocalFocusManager.current
    var name by rememberSaveable { mutableStateOf(profile.name) }
    var bio by rememberSaveable { mutableStateOf(profile.bio) }
    var initialized by rememberSaveable { mutableStateOf(profile.identity.isNotEmpty()) }
    LaunchedEffect(Unit) {
        viewModel.dismissError()
        viewModel.refresh()
    }
    // A restored entry may compose before the profile loads; fill once without replacing a draft.
    LaunchedEffect(profile) {
        if (!initialized && profile.identity.isNotEmpty()) {
            name = profile.name
            bio = profile.bio
            initialized = true
        }
    }
    val changed = name.trim() != profile.name || bio.trim() != profile.bio
    val editable = initialized && !busy

    val picker = rememberLauncherForActivityResult(ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null) {
            viewModel.dismissError()
            onChoosePhoto(uri)
        }
    }
    val choose = { picker.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)) }
    val chooseLabel = stringResource(R.string.profile_photo_choose)
    val removeLabel = stringResource(R.string.profile_photo_remove)
    val picture = profile.picture
    val openPhoto: () -> Unit = {
        if (picture == null) choose() else MediaViewer.open(listOf(pictureItem("profile-photo", picture, profile.name, actions = listOf(
            listOf(MenuAction(chooseLabel, R.drawable.oi_image) { MediaViewer.close(); choose() }),
            listOf(MenuAction(removeLabel, R.drawable.oi_trash, destructive = true) { MediaViewer.close(); viewModel.removePicture() }),
        ))))
    }

    GroupedScreen(
        { Text("Edit profile") },
        connectionStatus = false,
        actions = {
            if (changed || busy) TextButton(
                enabled = foreground && editable && name.isNotBlank(),
                onClick = { viewModel.saveDetails(name.trim(), bio.trim()) { focus.clearFocus() } },
            ) { Text(if (busy && changed) "Saving…" else "Save") }
        },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().consumeWindowInsets(padding).imePadding(),
            contentPadding = padding.listPadding(top = 8.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            item("photo") {
                Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.CenterHorizontally) {
                    val editLabel = stringResource(R.string.profile_photo_edit)
                    Box(Modifier.semantics { contentDescription = editLabel }) {
                        Avatar(profile.name.ifBlank { "?" }, size = 112.dp, identityKey = profile.identity.ifBlank { profile.name },
                            image = picture, onClick = if (busy) null else openPhoto, originKey = "profile-photo")
                        Box(
                            Modifier.align(Alignment.BottomEnd).offset(6.dp, 6.dp).size(40.dp)
                                .border(4.dp, c.groupedPage, CircleShape).padding(4.dp)
                                .clip(CircleShape).background(c.accentFill),
                            contentAlignment = Alignment.Center,
                        ) {
                            if (busy && !changed) CircularProgressIndicator(Modifier.size(16.dp), color = Color.White, strokeWidth = 2.dp)
                            else Icon(painterResource(R.drawable.oi_camera), null, Modifier.size(18.dp), tint = Color.White)
                        }
                    }
                    TextButton(onClick = choose, enabled = !busy, modifier = Modifier.padding(top = 4.dp)) {
                        Text(stringResource(R.string.profile_photo_change))
                    }
                }
            }
            item("name") {
                AppTextField(
                    value = name,
                    onValueChange = { if (it.length <= NameLimit) name = it },
                    label = "Name",
                    placeholder = "Your name",
                    enabled = editable,
                    keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Words, imeAction = ImeAction.Next),
                    keyboardActions = KeyboardActions(onNext = { focus.moveFocus(FocusDirection.Next) }),
                )
            }
            item("about") {
                AppTextField(
                    value = bio,
                    onValueChange = { if (it.length <= AboutLimit) bio = it },
                    label = "About",
                    placeholder = "A few words about you",
                    enabled = editable,
                    singleLine = false,
                    maxLines = 6,
                    suffix = { AppTextFieldCounter(bio.length, AboutLimit) },
                )
            }
            (work as? ProfileWork.Failed)?.let { failure ->
                item("error") {
                    Text(failure.reason, Modifier.padding(horizontal = 16.dp), style = FootnoteStyle, color = c.danger)
                }
            }
            item("identity") {
                SettingsCard {
                    SettingsRow(stringResource(R.string.identity_qr_title), icon = R.drawable.oi_qr_code,
                        onClick = { app.navigator.push(Routes.ShareIdentity) })
                    SettingsRow(stringResource(R.string.recovery_title), icon = R.drawable.i_key,
                        onClick = { app.navigator.push(Routes.RecoveryPhrase) })
                }
            }
            item("note") {
                Text("Your name, photo and about are visible to your contacts and to people you message.",
                    Modifier.padding(horizontal = 16.dp), style = FootnoteStyle, color = c.muted)
            }
        }
    }
}
