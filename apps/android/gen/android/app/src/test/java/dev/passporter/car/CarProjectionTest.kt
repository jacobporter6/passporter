package dev.passporter.car

import android.app.Application
import android.content.ContentProvider
import android.content.ContentValues
import android.content.pm.ProviderInfo
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf

/** Android Auto's connection provider, answering [rows] (`null`: no cursor at all). */
class FakeConnectionProvider : ContentProvider() {
    override fun onCreate() = true

    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, args: Array<out String>?, order: String?): Cursor? =
        rows?.let { values ->
            MatrixCursor(arrayOf(column)).apply { values.forEach { addRow(arrayOf<Any>(it)) } }
        }

    override fun getType(uri: Uri): String? = null

    override fun insert(uri: Uri, values: ContentValues?): Uri? = null

    override fun delete(uri: Uri, selection: String?, args: Array<out String>?) = 0

    override fun update(uri: Uri, values: ContentValues?, selection: String?, args: Array<out String>?) = 0

    companion object {
        var rows: List<Int>? = null
        var column = AndroidAutoProjection.COLUMN
    }
}

@RunWith(RobolectricTestRunner::class)
class CarProjectionTest {
    private val context: Application = RuntimeEnvironment.getApplication()

    private fun install(owner: String, rows: List<Int>?, column: String = AndroidAutoProjection.COLUMN) {
        FakeConnectionProvider.rows = rows
        FakeConnectionProvider.column = column
        val info = ProviderInfo().apply {
            authority = AndroidAutoProjection.AUTHORITY
            packageName = owner
            name = FakeConnectionProvider::class.java.name
        }
        shadowOf(context.packageManager).addOrUpdateProvider(info)
        Robolectric.buildContentProvider(FakeConnectionProvider::class.java).create(info)
    }

    private fun projecting() = AndroidAutoProjection(context).isProjecting()

    @Test
    fun withoutAndroidAutoThereIsNoCar() {
        assertFalse(projecting())
    }

    @Test
    fun androidAutoProjectingIsTheCar() {
        install(AndroidAutoProjection.AUTO_PACKAGE, listOf(AndroidAutoProjection.PROJECTION))
        assertTrue(projecting())
    }

    @Test
    fun aNativeCarOrNoConnectionIsNotProjection() {
        for (state in listOf(0, 1)) {
            install(AndroidAutoProjection.AUTO_PACKAGE, listOf(state))
            assertFalse("$state", projecting())
        }
    }

    @Test
    fun anAnswerThatSaysNothingIsNoCar() {
        install(AndroidAutoProjection.AUTO_PACKAGE, null)
        assertFalse("no cursor", projecting())
        install(AndroidAutoProjection.AUTO_PACKAGE, emptyList())
        assertFalse("no row", projecting())
        install(AndroidAutoProjection.AUTO_PACKAGE, listOf(AndroidAutoProjection.PROJECTION), column = "other")
        assertFalse("no column", projecting())
    }

    @Test
    fun anotherAppClaimingTheAuthorityIsNoCar() {
        install("com.example.impostor", listOf(AndroidAutoProjection.PROJECTION))
        assertFalse(projecting())
    }
}
