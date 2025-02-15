use std::cmp::Ordering;
use std::fmt::Display;

use serde::{Deserialize, Deserializer, Serialize};

use crate::consts::appconsts::SHARE_SIZE;
use crate::consts::data_availability_header::{
    MAX_EXTENDED_SQUARE_WIDTH, MIN_EXTENDED_SQUARE_WIDTH,
};
use crate::namespaced_data::{NamespacedData, NamespacedDataId};
use crate::nmt::{Namespace, NamespacedSha2Hasher, Nmt, NmtExt, NS_SIZE};
use crate::row::RowId;
use crate::{bail_validation, DataAvailabilityHeader, Error, Result};

/// Represents either column or row of the [`ExtendedDataSquare`].
///
/// [`ExtendedDataSquare`]: crate::rsmt2d::ExtendedDataSquare
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AxisType {
    /// A row of the data square.
    Row = 0,
    /// A column of the data square.
    Col,
}

impl TryFrom<u8> for AxisType {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(AxisType::Row),
            1 => Ok(AxisType::Col),
            n => Err(Error::InvalidAxis(n.into())),
        }
    }
}

impl Display for AxisType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AxisType::Row => write!(f, "Row"),
            AxisType::Col => write!(f, "Column"),
        }
    }
}

/// The data matrix in Celestia blocks extended with parity data.
///
/// It is created by a fixed size chunks of data, called [`Share`]s.

/// Each share is a cell of the [`ExtendedDataSquare`].
///
/// # Structure
///
/// The [`ExtendedDataSquare`] consists of four quadrants. The first
/// quadrant (upper-left) is the original data submitted to the network,
/// referred to as `OriginalDataSquare`. The other three quadrants are
/// the parity data encoded row-wise or column-wise using Reed-Solomon
/// `codec` specified in `EDS`.
///
/// The below diagram shows how the `EDS` is constructed. First, the 2nd
/// and 3rd quadrants are created by computing Reed-Solomon parity data
/// of the original data square, row-wise for 2nd and column-wise for
/// 3rd quadrant. Then, the 4th quadrant is computed either row-wise
/// from 3rd or column-wise from 2nd quadrant.
///
/// ```text
///  ---------------------------
/// |             |             |
/// |           --|->           |
/// |      1    --|->    2      |
/// |           --|->           |
/// |    | | |    |             |
///  -------------+-------------
/// |    v v v    |             |
/// |           --|->           |
/// |      3    --|->    4      |
/// |           --|->           |
/// |             |             |
///  ---------------------------
/// ```
///
/// # Data availability
///
/// The [`DataAvailabilityHeader`] is created by computing [`Nmt`] merkle
/// roots of each row and column of [`ExtendedDataSquare`].
/// By putting those together there are some key
/// properties those have in terms of data availability.
///
/// Thanks to the parity data, to make original data unrecoverable, a malicious
/// actor would need to hide more than a half of the data from each row and column.
/// If we take `k` as the width of the `OriginalDataSquare`, then the attacker
/// needs to hide more than `(k + 1)^2` shares from the [`ExtendedDataSquare`].
/// For the `EDS` with a width of 4, the attacker needs to hide more than 50% of
/// all the shares and that value approaches 25% as the square grows.
///
/// This allows for really efficient data sampling, as the sampling node can reach
/// very high confidence that whole data is available by taking only a few samples.
///
/// # Example
///
/// This example shows rebuilding the merkle trees for each row of the EDS and compares
/// them with the root hashes stored in data availability header.
///
/// ```no_run
/// use celestia_types::Share;
/// # use celestia_types::{ExtendedDataSquare, ExtendedHeader};
/// # fn get_header(_: usize) -> ExtendedHeader {
/// #     unimplemented!()
/// # }
/// # fn get_eds(_: usize) -> ExtendedDataSquare {
/// #     unimplemented!()
/// # }
///
/// let block_height = 15;
/// let header = get_header(block_height);
/// let eds = get_eds(block_height);
/// let width = header.dah.square_len();
///
/// // for each row of the data square, build an nmt
/// for row in 0..eds.square_len() {
///     // check if the root corresponds to the one from the dah
///     let root = eds.row_nmt(row).unwrap().root();
///     assert_eq!(root, header.dah.row_root(row).unwrap());
/// }
/// ```
///
/// [`Nmt`]: crate::nmt::Nmt
/// [`Share`]: crate::share::Share
/// [`DataAvailabilityHeader`]: crate::DataAvailabilityHeader
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtendedDataSquare {
    /// The raw data of the EDS.
    #[serde(with = "celestia_tendermint_proto::serializers::bytes::vec_base64string")]
    data_square: Vec<Vec<u8>>,
    /// The codec used to encode parity shares.
    codec: String,
    /// pre-calculated square length
    #[serde(skip)]
    square_len: usize,
}

impl ExtendedDataSquare {
    /// Create a new EDS out of the provided shares.
    ///
    /// Shares should be provided in a row-major order, i.e. first shares of the first row,
    /// then of the second row and so on.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///  - shares are of sizes different than [`SHARE_SIZE`]
    ///  - amount of shares doesn't allow for forming a square
    ///  - width of the square is smaller than [`MIN_EXTENDED_SQUARE_WIDTH`]
    ///  - width of the square is bigger than [`MAX_EXTENDED_SQUARE_WIDTH`]
    ///  - width of the square isn't a power of 2
    ///  - namespaces of shares aren't in non-decreasing order row and column wise
    pub fn new(shares: Vec<Vec<u8>>, codec: String) -> Result<Self> {
        const MIN_SHARES: usize = MIN_EXTENDED_SQUARE_WIDTH * MIN_EXTENDED_SQUARE_WIDTH;
        const MAX_SHARES: usize = MAX_EXTENDED_SQUARE_WIDTH * MAX_EXTENDED_SQUARE_WIDTH;

        if shares.len() < MIN_SHARES {
            bail_validation!(
                "shares len ({}) < MIN_SHARES ({})",
                shares.len(),
                MIN_SHARES
            );
        }
        if shares.len() > MAX_SHARES {
            bail_validation!(
                "shares len ({}) > MAX_SHARES ({})",
                shares.len(),
                MAX_SHARES
            );
        }

        let square_len = f64::sqrt(shares.len() as f64) as usize;

        if square_len * square_len != shares.len() {
            return Err(Error::EdsInvalidDimentions);
        }
        // must be a power of 2
        if square_len.count_ones() != 1 {
            return Err(Error::EdsInvalidDimentions);
        }

        let eds = ExtendedDataSquare {
            data_square: shares,
            codec,
            square_len,
        };

        let check_share = |row, col, prev_ns: Option<Namespace>, axis| -> Result<Namespace> {
            let share = eds.share(row, col)?;

            if share.len() != SHARE_SIZE {
                bail_validation!("share len ({}) != SHARE_SIZE ({})", share.len(), SHARE_SIZE);
            }

            let ns = if is_ods_square(row, col, eds.square_len()) {
                Namespace::from_raw(&share[..NS_SIZE])?
            } else {
                Namespace::PARITY_SHARE
            };

            if prev_ns.map_or(false, |prev_ns| ns < prev_ns) {
                let axis_idx = match axis {
                    AxisType::Row => row,
                    AxisType::Col => col,
                };
                bail_validation!("Shares of {axis} {axis_idx} are not sorted by their namespace");
            }

            Ok(ns)
        };

        // Validate that namespaces of each row are sorted
        for row in 0..eds.square_len() {
            let mut prev_ns = None;

            for col in 0..eds.square_len() {
                prev_ns = Some(check_share(row, col, prev_ns, AxisType::Row)?);
            }
        }
        // Validate that namespaces of each column are sorted
        for col in 0..eds.square_len() {
            let mut prev_ns = None;

            for row in 0..eds.square_len() {
                prev_ns = Some(check_share(row, col, prev_ns, AxisType::Col)?);
            }
        }

        Ok(eds)
    }

    /// Create a new EDS out of the provided original data square shares.
    ///
    /// This method is similar to the [`ExtendedDataSquare::new`] but parity data
    /// will be encoded automatically using the [`leopard_codec`]
    ///
    /// Shares should be provided in a row-major order.
    ///
    /// # Errors
    ///
    /// The same errors as in [`ExtendedDataSquare::new`] applies. The constrain
    /// will be checked after the parity data is generated.
    ///
    /// Additionally, this function will propagate any error from encoding parity data.
    pub fn from_ods(mut ods_shares: Vec<Vec<u8>>) -> Result<ExtendedDataSquare> {
        let ods_width = f64::sqrt(ods_shares.len() as f64) as usize;
        // this couldn't be detected later in `new()`
        if ods_width * ods_width != ods_shares.len() {
            return Err(Error::EdsInvalidDimentions);
        }

        let eds_width = ods_width * 2;
        let mut eds_shares = Vec::with_capacity(eds_width * eds_width);
        // take rows of ods and interleave them with parity shares
        for _ in 0..ods_width {
            eds_shares.extend(ods_shares.drain(..ods_width));
            for _ in 0..ods_width {
                eds_shares.push(vec![0; SHARE_SIZE]);
            }
        }
        // fill bottom half of the square with parity data
        eds_shares.resize(eds_width * eds_width, vec![0; SHARE_SIZE]);

        // 2nd quadrant - encode parity of rows of 1st quadrant
        for row in eds_shares.chunks_mut(eds_width).take(ods_width) {
            leopard_codec::encode(row, ods_width)?;
        }
        // 3rd quadrant - encode parity of columns of 1st quadrant
        for col in 0..ods_width {
            let mut col: Vec<_> = eds_shares.iter_mut().skip(col).step_by(eds_width).collect();
            leopard_codec::encode(&mut col, ods_width)?;
        }
        // 4th quadrant - encode parity of rows of 3rd quadrant
        for row in eds_shares.chunks_mut(eds_width).skip(ods_width) {
            leopard_codec::encode(row, ods_width)?;
        }

        ExtendedDataSquare::new(eds_shares, "leopard".to_string())
    }

    /// The raw data of the EDS.
    pub fn data_square(&self) -> &[Vec<u8>] {
        &self.data_square
    }

    /// The codec used to encode parity shares.
    pub fn codec(&self) -> &str {
        self.codec.as_str()
    }

    /// Returns the share of the provided coordinates.
    pub fn share(&self, row: usize, column: usize) -> Result<&[u8]> {
        let index = row * self.square_len + column;

        self.data_square
            .get(index)
            .map(Vec::as_slice)
            .ok_or(Error::EdsIndexOutOfRange(index))
    }

    /// Returns the mutable share of the provided coordinates.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn share_mut(&mut self, row: usize, column: usize) -> Result<&mut [u8]> {
        let index = row * self.square_len + column;

        self.data_square
            .get_mut(index)
            .map(Vec::as_mut_slice)
            .ok_or(Error::EdsIndexOutOfRange(index))
    }

    /// Returns the shares of a row.
    pub fn row(&self, index: usize) -> Result<Vec<Vec<u8>>> {
        self.axis(AxisType::Row, index)
    }

    /// Returns the [`Nmt`] of a row.
    pub fn row_nmt(&self, index: usize) -> Result<Nmt> {
        self.axis_nmt(AxisType::Row, index)
    }

    /// Returns the shares of a column.
    pub fn column(&self, index: usize) -> Result<Vec<Vec<u8>>> {
        self.axis(AxisType::Col, index)
    }

    /// Returns the [`Nmt`] of a column.
    pub fn column_nmt(&self, index: usize) -> Result<Nmt> {
        self.axis_nmt(AxisType::Col, index)
    }

    /// Returns the shares of column or row.
    pub fn axis(&self, axis: AxisType, index: usize) -> Result<Vec<Vec<u8>>> {
        (0..self.square_len)
            .map(|i| {
                let (row, col) = match axis {
                    AxisType::Row => (index, i),
                    AxisType::Col => (i, index),
                };

                self.share(row, col).map(ToOwned::to_owned)
            })
            .collect()
    }

    /// Returns the [`Nmt`] of column or row.
    pub fn axis_nmt(&self, axis: AxisType, index: usize) -> Result<Nmt> {
        let mut tree = Nmt::default();

        for i in 0..self.square_len {
            let (row, col) = match axis {
                AxisType::Row => (index, i),
                AxisType::Col => (i, index),
            };

            let share = self.share(row, col)?;

            let ns = if is_ods_square(col, row, self.square_len) {
                Namespace::from_raw(&share[..NS_SIZE])?
            } else {
                Namespace::PARITY_SHARE
            };

            tree.push_leaf(share, *ns).map_err(Error::Nmt)?;
        }

        Ok(tree)
    }

    /// Get EDS square length.
    pub fn square_len(&self) -> usize {
        self.square_len
    }

    /// Return all the shares that belong to the provided namespace in the EDS.
    /// Results are returned as a list of rows of shares with the inclusion proof.
    pub fn get_namespaced_data(
        &self,
        namespace: Namespace,
        dah: &DataAvailabilityHeader,
        height: u64,
    ) -> Result<Vec<NamespacedData>> {
        let mut data = Vec::new();

        for row in 0..self.square_len {
            let Some(row_root) = dah.row_root(row) else {
                break;
            };

            if !row_root.contains::<NamespacedSha2Hasher>(*namespace) {
                continue;
            }

            let mut shares = Vec::with_capacity(self.square_len);

            for col in 0..self.square_len {
                let share = self.share(row, col)?;

                let ns = if is_ods_square(row, col, self.square_len) {
                    Namespace::from_raw(&share[..NS_SIZE])?
                } else {
                    Namespace::PARITY_SHARE
                };

                // Shares in each row of EDS are sorted by namespace, so we
                // can stop search the row if we reach to a bigger namespace.
                match ns.cmp(&namespace) {
                    Ordering::Less => {}
                    Ordering::Equal => shares.push(share.to_owned()),
                    Ordering::Greater => break,
                }
            }

            let proof = self.row_nmt(row)?.get_namespace_proof(*namespace);
            let row = RowId::new(row as u16, height)?;
            let namespaced_data_id = NamespacedDataId { row, namespace };

            data.push(NamespacedData {
                namespaced_data_id,
                proof: proof.into(),
                shares,
            })
        }

        Ok(data)
    }
}

#[derive(Deserialize)]
struct RawExtendedDataSquare {
    #[serde(with = "celestia_tendermint_proto::serializers::bytes::vec_base64string")]
    pub data_square: Vec<Vec<u8>>,
    pub codec: String,
}

impl<'de> Deserialize<'de> for ExtendedDataSquare {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let eds = RawExtendedDataSquare::deserialize(deserializer)?;
        let share_number = eds.data_square.len();
        ExtendedDataSquare::new(eds.data_square, eds.codec).map_err(|_| {
            <D::Error as serde::de::Error>::invalid_length(
                share_number,
                &"number of shares must be a perfect square",
            )
        })
    }
}

/// Returns true if and only if the provided coordinates belongs to Original Data Square
/// (i.e. first quadrant of Extended Data Square).
pub(crate) fn is_ods_square(row: usize, column: usize, square_len: usize) -> bool {
    let half_square_len = square_len / 2;
    row < half_square_len && column < half_square_len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_type_serialization() {
        assert_eq!(AxisType::Row as u8, 0);
        assert_eq!(AxisType::Col as u8, 1);
    }

    #[test]
    fn axis_type_deserialization() {
        assert_eq!(AxisType::try_from(0).unwrap(), AxisType::Row);
        assert_eq!(AxisType::try_from(1).unwrap(), AxisType::Col);

        let axis_type_err = AxisType::try_from(2).unwrap_err();
        assert!(matches!(axis_type_err, Error::InvalidAxis(2)));
        let axis_type_err = AxisType::try_from(99).unwrap_err();
        assert!(matches!(axis_type_err, Error::InvalidAxis(99)));
    }

    #[test]
    fn get_namespaced_data() {
        let eds_json = include_str!("../test_data/shwap_samples/eds.json");
        let eds: ExtendedDataSquare = serde_json::from_str(eds_json).unwrap();

        let dah_json = include_str!("../test_data/shwap_samples/dah.json");
        let dah: DataAvailabilityHeader = serde_json::from_str(dah_json).unwrap();

        let height = 45577;

        let rows = eds
            .get_namespaced_data(Namespace::new_v0(&[1, 170]).unwrap(), &dah, height)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        row.verify(&dah).unwrap();
        assert_eq!(row.shares.len(), 2);

        let rows = eds
            .get_namespaced_data(Namespace::new_v0(&[1, 187]).unwrap(), &dah, height)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].shares.len(), 1);
        assert_eq!(rows[1].shares.len(), 4);
        for row in rows {
            row.verify(&dah).unwrap();
        }
    }

    #[test]
    fn nmt_roots() {
        let eds_json = include_str!("../test_data/shwap_samples/eds.json");
        let eds: ExtendedDataSquare = serde_json::from_str(eds_json).unwrap();

        let dah_json = include_str!("../test_data/shwap_samples/dah.json");
        let dah: DataAvailabilityHeader = serde_json::from_str(dah_json).unwrap();

        assert_eq!(dah.row_roots.len(), eds.square_len());
        assert_eq!(dah.column_roots.len(), eds.square_len());

        for (i, root) in dah.row_roots.iter().enumerate() {
            let mut tree = eds.row_nmt(i).unwrap();
            assert_eq!(*root, tree.root());

            let mut tree = eds.axis_nmt(AxisType::Row, i).unwrap();
            assert_eq!(*root, tree.root());
        }

        for (i, root) in dah.column_roots.iter().enumerate() {
            let mut tree = eds.column_nmt(i).unwrap();
            assert_eq!(*root, tree.root());

            let mut tree = eds.axis_nmt(AxisType::Col, i).unwrap();
            assert_eq!(*root, tree.root());
        }
    }

    #[test]
    fn ods_square() {
        assert!(is_ods_square(0, 0, 4));
        assert!(is_ods_square(0, 1, 4));
        assert!(is_ods_square(1, 0, 4));
        assert!(is_ods_square(1, 1, 4));

        assert!(!is_ods_square(0, 2, 4));
        assert!(!is_ods_square(0, 3, 4));
        assert!(!is_ods_square(1, 2, 4));
        assert!(!is_ods_square(1, 3, 4));

        assert!(!is_ods_square(2, 0, 4));
        assert!(!is_ods_square(2, 1, 4));
        assert!(!is_ods_square(3, 0, 4));
        assert!(!is_ods_square(3, 1, 4));

        assert!(!is_ods_square(2, 2, 4));
        assert!(!is_ods_square(2, 3, 4));
        assert!(!is_ods_square(3, 2, 4));
        assert!(!is_ods_square(3, 3, 4));
    }

    #[test]
    fn get_row_and_col() {
        let share = |x, y| {
            [
                Namespace::new_v0(&[x, y]).unwrap().as_bytes(),
                &[0u8; SHARE_SIZE - NS_SIZE][..],
            ]
            .concat()
        };

        #[rustfmt::skip]
        let shares = vec![
            share(0, 0), share(0, 1), share(0, 2), share(0, 3),
            share(1, 0), share(1, 1), share(1, 2), share(1, 3),
            share(2, 0), share(2, 1), share(2, 2), share(2, 3),
            share(3, 0), share(3, 1), share(3, 2), share(3, 3),
        ];

        let eds = ExtendedDataSquare::new(shares, "fake".to_string()).unwrap();

        assert_eq!(
            eds.row(0).unwrap(),
            vec![share(0, 0), share(0, 1), share(0, 2), share(0, 3)]
        );
        assert_eq!(
            eds.row(1).unwrap(),
            vec![share(1, 0), share(1, 1), share(1, 2), share(1, 3)]
        );
        assert_eq!(
            eds.row(2).unwrap(),
            vec![share(2, 0), share(2, 1), share(2, 2), share(2, 3)]
        );
        assert_eq!(
            eds.row(3).unwrap(),
            vec![share(3, 0), share(3, 1), share(3, 2), share(3, 3)]
        );

        assert_eq!(
            eds.axis(AxisType::Row, 0).unwrap(),
            vec![share(0, 0), share(0, 1), share(0, 2), share(0, 3)]
        );
        assert_eq!(
            eds.axis(AxisType::Row, 1).unwrap(),
            vec![share(1, 0), share(1, 1), share(1, 2), share(1, 3)]
        );
        assert_eq!(
            eds.axis(AxisType::Row, 2).unwrap(),
            vec![share(2, 0), share(2, 1), share(2, 2), share(2, 3)]
        );
        assert_eq!(
            eds.axis(AxisType::Row, 3).unwrap(),
            vec![share(3, 0), share(3, 1), share(3, 2), share(3, 3)]
        );

        assert_eq!(
            eds.column(0).unwrap(),
            vec![share(0, 0), share(1, 0), share(2, 0), share(3, 0)]
        );
        assert_eq!(
            eds.column(1).unwrap(),
            vec![share(0, 1), share(1, 1), share(2, 1), share(3, 1)]
        );
        assert_eq!(
            eds.column(2).unwrap(),
            vec![share(0, 2), share(1, 2), share(2, 2), share(3, 2)]
        );
        assert_eq!(
            eds.column(3).unwrap(),
            vec![share(0, 3), share(1, 3), share(2, 3), share(3, 3)]
        );

        assert_eq!(
            eds.axis(AxisType::Col, 0).unwrap(),
            vec![share(0, 0), share(1, 0), share(2, 0), share(3, 0)]
        );
        assert_eq!(
            eds.axis(AxisType::Col, 1).unwrap(),
            vec![share(0, 1), share(1, 1), share(2, 1), share(3, 1)]
        );
        assert_eq!(
            eds.axis(AxisType::Col, 2).unwrap(),
            vec![share(0, 2), share(1, 2), share(2, 2), share(3, 2)]
        );
        assert_eq!(
            eds.axis(AxisType::Col, 3).unwrap(),
            vec![share(0, 3), share(1, 3), share(2, 3), share(3, 3)]
        );
    }

    #[test]
    fn validation() {
        ExtendedDataSquare::new(vec![], "fake".to_string()).unwrap_err();
        ExtendedDataSquare::new(vec![vec![]], "fake".to_string()).unwrap_err();
        ExtendedDataSquare::new(vec![vec![]; 4], "fake".to_string()).unwrap_err();

        ExtendedDataSquare::new(vec![vec![0u8; SHARE_SIZE]; 4], "fake".to_string()).unwrap();
        ExtendedDataSquare::new(vec![vec![0u8; SHARE_SIZE]; 6], "fake".to_string()).unwrap_err();
        ExtendedDataSquare::new(vec![vec![0u8; SHARE_SIZE]; 16], "fake".to_string()).unwrap();

        let share = |n| {
            [
                Namespace::new_v0(&[n]).unwrap().as_bytes(),
                &[0u8; SHARE_SIZE - NS_SIZE][..],
            ]
            .concat()
        };

        ExtendedDataSquare::from_ods(vec![
            // row 0
            share(0), // ODS
        ])
        .unwrap();

        ExtendedDataSquare::from_ods(vec![
            // row 0
            share(1),
            share(2),
            // row 1
            share(1),
            share(3),
        ])
        .unwrap();

        ExtendedDataSquare::from_ods(vec![
            // row 0
            share(1),
            share(2),
            // row 1
            share(1),
            share(1), // error: smaller namespace in 2nd column
        ])
        .unwrap_err();

        ExtendedDataSquare::from_ods(vec![
            // row 0
            share(1),
            share(1),
            // row 1
            share(2),
            share(1), // error: smaller namespace in 2nd row
        ])
        .unwrap_err();

        // not a power of 2
        ExtendedDataSquare::new(vec![share(1); 6 * 6], "fake".to_string()).unwrap_err();

        // too big
        // we need to go to the next power of 2 or we just hit other checks
        let square_width = MAX_EXTENDED_SQUARE_WIDTH * 2;
        ExtendedDataSquare::new(vec![share(1); square_width.pow(2)], "fake".to_string())
            .unwrap_err();
    }
}
