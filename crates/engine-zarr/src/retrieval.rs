//! Serial plain-Zarr and outer-transformed Icechunk reads. Keep temporary
//! encoded/intermediate admission only until each stored chunk finishes.
use ds_core::{deadline, error::DataServerError};
use unsafe_cell_slice::UnsafeCellSlice;
use zarrs::array::{Array, ArrayBytes, ArrayBytesFixedDisjointView, ArraySubset, CodecOptions};

use crate::{catalog::chunk_read_error, encoded, store::EngineStore};

pub(crate) fn serial(
    array: &Array<EngineStore>,
    subset: &ArraySubset,
    options: &CodecOptions,
) -> Result<ArrayBytes<'static>, DataServerError> {
    deadline::check()?;
    let retrieve = || {
        let result = array.retrieve_array_subset_opt::<ArrayBytes<'static>>(subset, options);
        deadline::check()?;
        result.map(ArrayBytes::into_owned).map_err(chunk_read_error)
    };
    // Coordinate/metadata reads keep their existing path, outside source
    // admission. Variable callers already hold native/source reservations.
    let Some(context) = encoded::current() else {
        return retrieve();
    };
    let chunks = array
        .chunks_in_array_subset(subset)
        .map_err(|error| chunk_read_error(error.into()))?;
    let Some(chunks) = chunks.filter(|chunks| chunks.indices().into_iter().take(2).count() > 1)
    else {
        // Preserve the single-chunk fast path, including missing/empty subsets
        // and upstream validation of unsupported subsets. Own the result before
        // releasing admission for storage/codec copies.
        let _encoded = encoded::enter(Some(context.budget.clone()));
        return retrieve();
    };
    let size = array.data_type().fixed_size().ok_or_else(|| {
        DataServerError::Engine("Zarr serial retrieval requires a fixed numeric type".into())
    })?;
    let length = subset.shape().iter().try_fold(size, |n, &dim| {
        usize::try_from(dim)
            .ok()
            .and_then(|dim| n.checked_mul(dim))
            .ok_or_else(|| context.budget.reject())
    })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| context.budget.reject())?;
    output.resize(length, 0);
    for indices in chunks.indices() {
        deadline::check()?;
        let overlap = array
            .chunk_subset(&indices)
            .map_err(chunk_read_error)?
            .overlap(subset)
            .map_err(|error| chunk_read_error(error.into()))?;
        let target = overlap
            .relative_to(subset.start())
            .map_err(|error| chunk_read_error(error.into()))?;
        let _encoded = encoded::enter(Some(context.budget.clone()));
        // SAFETY: this is the only live view of output. Its exclusive mutable
        // borrow ends with this synchronous call, before the next iteration.
        // The checked byte length above also bounds zarrs' shape arithmetic.
        let mut view = unsafe {
            ArrayBytesFixedDisjointView::new(
                UnsafeCellSlice::new(&mut output),
                size,
                subset.shape(),
                target,
            )
        }
        .map_err(|error| chunk_read_error(zarrs::array::CodecError::from(error).into()))?;
        // Keep zarrs' decode-into path: no additional native chunk buffer/copy,
        // inner-chunk splitting, payload reads, or full-shard decode expansion.
        let result = array.retrieve_array_subset_into_opt(&overlap, (&mut view).into(), options);
        deadline::check()?;
        result.map_err(chunk_read_error)?;
        // All temporary decoder handles and codec/index copies have dropped.
        // Only the native output remains, covered by the caller's reservation.
    }
    Ok(ArrayBytes::new_flen(output))
}

#[cfg(test)]
mod tests;
