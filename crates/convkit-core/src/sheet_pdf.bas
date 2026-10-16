Option Explicit

' Lays a workbook out for PDF, then exports it: widens each column whose
' numbers or dates would print as ###, and scales a sheet wider than its page
' down to the page width. What it did goes to convkit-sheets.txt in the
' profile, one key=value per line, which conv reads for its notes. See
' sheet_pdf.rs.
'
' conv calls ToPdf with both paths absolute.
'
' LibreOffice runs this headless, where an error nothing catches opens a
' dialog nobody sees and soffice never exits. So ToPdf catches everything
' below it, and every name here is plain: a reserved word as a name (Base,
' for one) is a compile error, which nothing can catch.

' Room added past the widest value when a column is widened, in 1/100 mm:
' the 2 mm LibreOffice's own optimal width adds.
Const SPARE = 200
' Cells measured in each run of cells sharing a format, beyond its largest
' and smallest value: every cell, or an even sample of a long run, which
' catches a format whose width varies otherwise (month names).
Const SAMPLES = 200

' Set up by Fit for the measuring below.
Private device As Object, formatter As Object, calc As Object, numbers As Object
' The widths the cells measured so far need, in 1/100 mm: to show without ###,
' and to show in full.
Private widest As Long, full As Long, fullest As Long
' The page styles this run set to fit the page width, as |name|name|.
Private fitted As String

Sub ToPdf(inPath As String, outPath As String)
	On Error GoTo Failed
	Dim doc As Object, changes As String
	doc = LoadCopy(inPath)
	If IsNull(doc) Then
		Report("status=unloaded")
		Exit Sub
	End If
	changes = Fit(doc)
	If changes = "" Then
		' A half-done fit is worse than none: start again from the file.
		doc.close(True)
		doc = LoadCopy(inPath)
		If IsNull(doc) Then
			Report("status=unloaded")
			Exit Sub
		End If
		changes = "fit=failed"
	End If
	If Export(doc, outPath) Then
		Report("status=exported" & Chr(10) & changes)
	Else
		Report("status=failed")
	End If
	doc.close(True)
	Exit Sub
Failed:
	On Error Resume Next
	Report("status=failed")
	If Not IsNull(doc) Then doc.close(True)
End Sub

' Opens a copy of the file, so the original is never opened for editing,
' which would leave a lock file beside it. Nothing when it cannot be read.
Function LoadCopy(inPath As String) As Object
	On Error GoTo Failed
	Dim source As String, folder As String, copy As String, parts() As String
	source = ConvertToURL(inPath)
	parts = Split(source, "/")
	folder = UserDir() & "/convkit-source"
	If Not FileExists(folder) Then MkDir(folder)
	copy = folder & "/" & parts(UBound(parts))
	FileCopy(source, copy)
	' As --convert-to opens a file, except for editing: hidden, without
	' asking anything, and without running the file's own macros.
	Dim args(2) As New com.sun.star.beans.PropertyValue
	args(0).Name = "Hidden"
	args(0).Value = True
	args(1).Name = "Silent"
	args(1).Value = True
	args(2).Name = "MacroExecutionMode"
	args(2).Value = com.sun.star.document.MacroExecMode.NEVER_EXECUTE
	LoadCopy = StarDesktop.loadComponentFromURL(copy, "_blank", 0, args())
	Exit Function
Failed:
	LoadCopy = Nothing
End Function

Function Export(doc As Object, outPath As String) As Boolean
	On Error GoTo Failed
	Dim args(1) As New com.sun.star.beans.PropertyValue
	args(0).Name = "FilterName"
	args(0).Value = "calc_pdf_Export"
	args(1).Name = "Overwrite"
	args(1).Value = True
	doc.storeToURL(ConvertToURL(outPath), args())
	Export = True
	Exit Function
Failed:
	Export = False
End Function


Function UserDir() As String
	UserDir = CreateUnoService("com.sun.star.util.PathSubstitution").substituteVariables("$(user)", True)
End Function

Sub Report(text As String)
	Dim n As Integer
	n = FreeFile
	Open ConvertFromURL(UserDir() & "/convkit-sheets.txt") For Output As #n
	Print #n, text
	Close #n
End Sub

' Fits every sheet and says what changed, or returns "" when it failed
' partway.
Function Fit(doc As Object) As String
	On Error GoTo Failed
	' Text is measured on the device LibreOffice lays the printed page out
	' with, so a width here is the width it decides ### by.
	device = doc.ReferenceDevice
	numbers = doc.NumberFormats
	formatter = CreateUnoService("com.sun.star.util.NumberFormatter")
	formatter.attachNumberFormatsSupplier(doc)
	calc = CreateUnoService("com.sun.star.sheet.FunctionAccess")
	fitted = "|"
	Dim i As Long, widened As Long, scaled As Long, smallest As Long, scale As Long
	smallest = 100
	For i = 0 To doc.Sheets.Count - 1
		widened = widened + Widen(doc, doc.Sheets.getByIndex(i))
	Next i
	For i = 0 To doc.Sheets.Count - 1
		scale = Shrink(doc, doc.Sheets.getByIndex(i))
		If scale < 100 Then
			scaled = scaled + 1
			If scale < smallest Then smallest = scale
		End If
	Next i
	Fit = "widened=" & widened & Chr(10) & "scaled=" & scaled & Chr(10) & "smallest=" & smallest
	Exit Function
Failed:
	Fit = ""
End Function

' The cells holding anything, as one range from A1, or Nothing for an empty
' sheet. Formatting alone does not count: LibreOffice does not print it.
Function Content(sheet As Object) As Object
	Dim found As Object, i As Long, a As Object, lastCol As Long, lastRow As Long
	found = sheet.queryContentCells(1 + 2 + 4 + 16)
	If found.Count = 0 Then
		Content = Nothing
		Exit Function
	End If
	For i = 0 To found.Count - 1
		a = found.getByIndex(i).RangeAddress
		If a.EndColumn > lastCol Then lastCol = a.EndColumn
		If a.EndRow > lastRow Then lastRow = a.EndRow
	Next i
	Content = sheet.getCellRangeByPosition(0, 0, lastCol, lastRow)
End Function

' Widens the columns of `sheet` holding a number, date or error that does not
' fit, and so prints as ###. Returns how many it widened.
Function Widen(doc As Object, sheet As Object) As Long
	If Not sheet.IsVisible Then Exit Function
	Dim used As Object
	used = Content(sheet)
	If IsNull(used) Then Exit Function
	Dim room As Long, c As Long, col As Object, need As Long, count As Long
	room = Printable(doc, sheet)
	For c = 0 To used.RangeAddress.EndColumn
		col = sheet.Columns.getByIndex(c)
		If col.IsVisible Then
			fullest = 0
			need = Needed(sheet, sheet.getCellRangeByPosition(c, 0, c, used.RangeAddress.EndRow))
			If need > col.Width Then
				' Wide enough for every value in full, not just to lose the ###.
				need = fullest + SPARE
				If need > room Then need = room
				If need > col.Width Then
					col.Width = need
					count = count + 1
				End If
			End If
		End If
	Next c
	Widen = count
End Function

' The width the widest number, date or error in a one-column range needs to
' show without ###, margins included, in 1/100 mm; 0 when it holds none.
' Leaves `fullest` at the width showing each of them in full needs.
Function Needed(sheet As Object, column As Object) As Long
	Dim groups As Object, g As Long, most As Long, w As Long
	groups = column.getUniqueCellFormatRanges()
	For g = 0 To groups.Count - 1
		w = GroupNeeded(sheet, groups.getByIndex(g))
		If w > most Then most = w
	Next g
	' An error value prints as ### too when it does not fit.
	Dim found As Object, i As Long, a As Object, row As Long, cell As Object, n As Long
	found = column.queryFormulaCells(4)
	For i = 0 To found.Count - 1
		a = found.getByIndex(i).RangeAddress
		For row = a.StartRow To a.EndRow
			If n >= SAMPLES Then Exit For
			cell = sheet.getCellByPosition(a.StartColumn, row)
			If Measurable(cell) Then
				w = FontOf(cell).getStringWidth(cell.String) + Margins(cell)
				If w > most Then most = w
				If w > fullest Then fullest = w
			End If
			n = n + 1
		Next row
	Next i
	Needed = most
End Function

' Whether a cell, or cells sharing a format, can print as ### for being too
' narrow. Shrink to fit makes the text smaller instead; turned or stacked
' text is not as wide as it is long; a merged cell has its whole area.
Function Measurable(props As Object) As Boolean
	Measurable = Not (props.ShrinkToFit Or props.RotateAngle <> 0 Or props.Orientation <> 0 Or props.getIsMerged())
End Function

' Needed, for cells sharing one format: measured for the largest and
' smallest value, and for every cell or an even sample of them.
Function GroupNeeded(sheet As Object, group As Object) As Long
	Dim addrs() As Object, r As Long, total As Long, range As Object
	addrs = group.getRangeAddresses()
	range = sheet.getCellRangeByPosition(addrs(0).StartColumn, addrs(0).StartRow, addrs(0).EndColumn, addrs(0).EndRow)
	If Not Measurable(range) Then Exit Function
	Dim key As Long, f As Object, general As Boolean, font As Object
	key = group.NumberFormat
	f = numbers.getByKey(key)
	general = f.StandardFormat And f.Type = com.sun.star.util.NumberFormat.NUMBER
	font = FontOf(group)
	widest = 0
	full = 0
	For r = 0 To UBound(addrs)
		total = total + addrs(r).EndRow - addrs(r).StartRow + 1
	Next r
	Dim stride As Double, pos As Double, offset As Long, rows As Long, cell As Object
	stride = 1
	If total > SAMPLES Then stride = total / SAMPLES
	For r = 0 To UBound(addrs)
		range = sheet.getCellRangeByPosition(addrs(r).StartColumn, addrs(r).StartRow, addrs(r).EndColumn, addrs(r).EndRow)
		rows = addrs(r).EndRow - addrs(r).StartRow + 1
		Measure(font, key, Extreme(4, range), general)
		Measure(font, key, Extreme(5, range), general)
		Do While pos < offset + rows
			cell = range.getCellByPosition(0, Int(pos - offset))
			If cell.Type = com.sun.star.table.CellContentType.VALUE Or cell.Type = com.sun.star.table.CellContentType.FORMULA Then
				Measure(font, key, cell.Value, general)
			End If
			pos = pos + stride
		Loop
		offset = offset + rows
	Next r
	If widest > 0 Then
		GroupNeeded = widest + Margins(group)
		If full + Margins(group) > fullest Then fullest = full + Margins(group)
	End If
End Function

' The largest (4) or smallest (5) number in `range`, errors left out; Empty
' when it holds none.
Function Extreme(which As Integer, range As Object) As Variant
	On Error GoTo Failed
	Extreme = calc.callFunction("AGGREGATE", Array(which, 6, range))
	Exit Function
Failed:
	Extreme = Empty
End Function

' Updates `widest` and `full` with how wide `value` prints in format `key`.
' General shows fewer decimals, or fewer digits as a power of ten, before it
' gives up and shows ###, so for it `widest` takes the narrowest way it can
' show the value, and `full` the whole number.
Sub Measure(font As Object, key As Long, value As Variant, general As Boolean)
	If IsEmpty(value) Then Exit Sub
	Dim w As Long
	If Not general Then
		w = font.getStringWidth(formatter.convertNumberToString(key, value))
		If w > widest Then widest = w
		If w > full Then full = w
		Exit Sub
	End If
	Dim size As Double, power As Long, digit As Long, whole As String, brief As String
	size = Abs(value)
	whole = CStr(Fix(size + 0.5))
	If size < 99999.5 Then
		brief = whole
	Else
		power = Int(Log(size) / Log(10))
		digit = Fix(size / 10 ^ power + 0.5)
		If digit = 10 Then
			digit = 1
			power = power + 1
		End If
		brief = digit & "E+" & Format(power, "00")
		If size >= 1E15 Then whole = brief
	End If
	If value < 0 Then
		whole = "-" & whole
		brief = "-" & brief
	End If
	w = font.getStringWidth(brief)
	If w > widest Then widest = w
	w = font.getStringWidth(whole)
	If w > full Then full = w
End Sub

' The font `props` (a cell, or cells sharing a format) prints in.
Function FontOf(props As Object) As Object
	Dim fd As New com.sun.star.awt.FontDescriptor
	fd.Name = props.CharFontName
	fd.Height = props.CharHeight * 2540 / 72
	fd.Weight = props.CharWeight
	fd.Slant = props.CharPosture
	fd.Kerning = False
	FontOf = device.getFont(fd)
End Function

Function Margins(props As Object) As Long
	Margins = props.ParaLeftMargin + props.ParaRightMargin + props.ParaIndent
End Function

' The width a page of `sheet` has for cells, in 1/100 mm.
Function Printable(doc As Object, sheet As Object) As Long
	Dim style As Object
	style = doc.StyleFamilies.getByName("PageStyles").getByName(sheet.PageStyle)
	Printable = style.Width - style.LeftMargin - style.RightMargin
End Function

' Scales a sheet wider than its page down to the page width, unless the file
' already says how to scale it or what to print. Returns the scale, in
' percent. A sheet narrower than its page is never enlarged, even when it
' shares its page style with one that is scaled.
Function Shrink(doc As Object, sheet As Object) As Long
	Shrink = 100
	If Not sheet.IsVisible Then Exit Function
	If UBound(sheet.getPrintAreas()) >= 0 Then Exit Function
	Dim style As Object, ours As Boolean
	style = doc.StyleFamilies.getByName("PageStyles").getByName(sheet.PageStyle)
	ours = InStr(fitted, "|" & sheet.PageStyle & "|") > 0
	If Not ours And (style.PageScale <> 100 Or style.ScaleToPages <> 0 Or style.ScaleToPagesX <> 0 Or style.ScaleToPagesY <> 0) Then Exit Function
	Dim used As Object
	used = Content(sheet)
	If IsNull(used) Then Exit Function
	Dim c As Long, wide As Long, col As Object, room As Long
	For c = 0 To used.RangeAddress.EndColumn
		col = sheet.Columns.getByIndex(c)
		If col.IsVisible Then wide = wide + col.Width
	Next c
	room = style.Width - style.LeftMargin - style.RightMargin
	If wide <= room Then Exit Function
	style.ScaleToPagesX = 1
	style.ScaleToPagesY = 0
	fitted = fitted & sheet.PageStyle & "|"
	' LibreOffice prints no smaller than 10%.
	Shrink = Int(room * 100 / wide)
	If Shrink < 10 Then Shrink = 10
End Function
