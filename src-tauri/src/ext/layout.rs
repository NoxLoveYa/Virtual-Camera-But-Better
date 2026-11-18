//! Uniform layouts reflected from a translated shader, and packing of values into them.
//!
//! The offsets come from `naga`'s module (the WGSL "internal layout of values" rules), never from a rule written
//! here. `naga`'s HLSL writer pads its cbuffer structs explicitly so that FXC places every member where WGSL does;
//! the spike's tests read the offsets back from the compiled bytecode (`D3DReflect`) and compare.

use std::fmt;

/// The scalar kinds a uniform member can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarKind {
    F32,
    I32,
    U32,
}

impl ScalarKind {
    fn name(self) -> &'static str {
        match self {
            ScalarKind::F32 => "f32",
            ScalarKind::I32 => "i32",
            ScalarKind::U32 => "u32",
        }
    }
}

/// The type of a uniform member, with the sizes WGSL gives it.
#[derive(Clone, Debug, PartialEq)]
pub enum UType {
    Scalar(ScalarKind),
    /// `vecN<kind>`, `n` in 2..=4.
    Vector { n: u8, kind: ScalarKind },
    /// `matCxR<f32>`: `columns` columns of `rows` rows (WGSL order: `mat3x2` has 3 columns of 2 rows). Memory holds
    /// the columns one after another, each padded to `column_stride` bytes.
    Matrix { columns: u8, rows: u8 },
    /// `array<elem, len>`; `stride` is the distance between elements.
    Array { len: u32, stride: u32, elem: Box<UType> },
    /// A struct; `size` is its WGSL size (rounded up to its alignment).
    Struct { name: String, size: u32, members: Vec<Member> },
}

/// One struct member: its name and where it sits inside the struct.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub name: String,
    pub offset: u32,
    pub ty: UType,
}

impl UType {
    /// Bytes between two columns of a matrix with `rows` rows (the alignment of its column vector).
    fn column_stride(rows: u8) -> u32 {
        if rows == 2 {
            8
        } else {
            16
        }
    }

    /// The size in bytes (WGSL "SizeOf").
    pub fn size(&self) -> u32 {
        match self {
            UType::Scalar(_) => 4,
            UType::Vector { n, .. } => 4 * *n as u32,
            UType::Matrix { columns, rows } => *columns as u32 * Self::column_stride(*rows),
            UType::Array { len, stride, .. } => len * stride,
            UType::Struct { size, .. } => *size,
        }
    }

    /// The canonical WGSL spelling, used to compare a manifest declaration with the shader's struct.
    pub fn wgsl(&self) -> String {
        match self {
            UType::Scalar(k) => k.name().into(),
            UType::Vector { n, kind } => format!("vec{n}<{}>", kind.name()),
            UType::Matrix { columns, rows } => format!("mat{columns}x{rows}<f32>"),
            UType::Array { len, elem, .. } => format!("array<{},{len}>", elem.wgsl()),
            UType::Struct { name, .. } => name.clone(),
        }
    }
}

/// A uniform buffer's struct: what the shader declares, reflected.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// The WGSL struct name.
    pub name: String,
    /// The struct's WGSL size in bytes.
    pub size: u32,
    pub members: Vec<Member>,
}

/// A value to pack. Vectors, matrices (columns first) and arrays are lists; a struct is a list of named members.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    F32(f32),
    I32(i32),
    U32(u32),
    List(Vec<Value>),
    Struct(Vec<(String, Value)>),
}

impl Value {
    pub fn vec<const N: usize>(v: [f32; N]) -> Value {
        Value::List(v.iter().map(|x| Value::F32(*x)).collect())
    }

    /// A matrix from its columns (WGSL order).
    pub fn mat<const C: usize, const R: usize>(columns: [[f32; R]; C]) -> Value {
        Value::List(columns.iter().map(|c| Value::vec(*c)).collect())
    }

    pub fn list(items: impl IntoIterator<Item = Value>) -> Value {
        Value::List(items.into_iter().collect())
    }

    pub fn structure<'a>(items: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
        Value::Struct(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Value {
        Value::F32(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Value {
        Value::I32(v)
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Value {
        Value::U32(v)
    }
}

/// A value that does not fit the layout.
#[derive(Clone, Debug, PartialEq)]
pub struct PackError(pub String);

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PackError {}

impl Layout {
    /// The size of the D3D11 constant buffer to bind: the struct size rounded up to 16 (constant buffers are made of
    /// 16-byte registers). Never 0.
    pub fn buffer_size(&self) -> usize {
        (self.size as usize).div_ceil(16).max(1) * 16
    }

    /// The bytes of the constant buffer (`buffer_size` long) for `values`, one per member by name. A member whose
    /// name starts with `_` (padding) may be left out and stays zero. A missing, unknown or mistyped value is an
    /// error, not a silent zero.
    pub fn pack(&self, values: &[(&str, Value)]) -> Result<Vec<u8>, PackError> {
        let mut bytes = vec![0u8; self.buffer_size()];
        let root = Value::Struct(values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
        let ty = UType::Struct {
            name: self.name.clone(),
            size: self.size,
            members: self.members.clone(),
        };
        write_value(&ty, &root, &mut bytes, 0, &self.name)?;
        Ok(bytes)
    }
}

fn put(bytes: &mut [u8], at: usize, v: [u8; 4], path: &str) -> Result<(), PackError> {
    bytes
        .get_mut(at..at + 4)
        .ok_or_else(|| PackError(format!("{path}: offset {at} is outside the buffer")))?
        .copy_from_slice(&v);
    Ok(())
}

fn write_value(ty: &UType, v: &Value, bytes: &mut [u8], at: usize, path: &str) -> Result<(), PackError> {
    let mismatch = |want: &str| PackError(format!("{path}: expected {want}, got {v:?}"));
    match (ty, v) {
        (UType::Scalar(ScalarKind::F32), Value::F32(x)) => put(bytes, at, x.to_le_bytes(), path),
        (UType::Scalar(ScalarKind::I32), Value::I32(x)) => put(bytes, at, x.to_le_bytes(), path),
        (UType::Scalar(ScalarKind::U32), Value::U32(x)) => put(bytes, at, x.to_le_bytes(), path),
        (UType::Scalar(k), _) => Err(mismatch(k.name())),
        (UType::Vector { n, kind }, Value::List(items)) if items.len() == *n as usize => {
            for (i, item) in items.iter().enumerate() {
                write_value(&UType::Scalar(*kind), item, bytes, at + 4 * i, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        (UType::Vector { n, kind }, _) => Err(mismatch(&format!("a list of {n} {}", kind.name()))),
        (UType::Matrix { columns, rows }, Value::List(cols)) if cols.len() == *columns as usize => {
            let stride = UType::column_stride(*rows) as usize;
            let column = UType::Vector {
                n: *rows,
                kind: ScalarKind::F32,
            };
            for (i, col) in cols.iter().enumerate() {
                write_value(&column, col, bytes, at + stride * i, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        (UType::Matrix { columns, rows }, _) => {
            Err(mismatch(&format!("{columns} columns of {rows} f32 (a list of lists)")))
        }
        (UType::Array { len, stride, elem }, Value::List(items)) if items.len() == *len as usize => {
            for (i, item) in items.iter().enumerate() {
                write_value(elem, item, bytes, at + *stride as usize * i, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        (UType::Array { len, .. }, _) => Err(mismatch(&format!("a list of {len} elements"))),
        (UType::Struct { members, .. }, Value::Struct(fields)) => {
            for (name, _) in fields {
                if !members.iter().any(|m| &m.name == name) {
                    return Err(PackError(format!("{path}: has no member `{name}`")));
                }
            }
            for m in members {
                let given = fields.iter().find(|(n, _)| *n == m.name);
                match given {
                    Some((_, value)) => {
                        write_value(&m.ty, value, bytes, at + m.offset as usize, &format!("{path}.{}", m.name))?
                    }
                    None if m.name.starts_with('_') => {}
                    None => return Err(PackError(format!("{path}: no value for member `{}`", m.name))),
                }
            }
            Ok(())
        }
        (UType::Struct { name, .. }, _) => Err(mismatch(&format!("struct {name} (named members)"))),
    }
}
