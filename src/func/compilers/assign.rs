use crate::{
    common::types::IntoOpaqueType,
    common::{
        types::{BasicType, ComplexType},
        Result, TypedValue,
    },
    compiler::llvm::Insn,
    compiler_error,
    func::{utils::FunctionCompilerUtils, *},
};

use super::ExprCompiler;

pub trait AssignmentCompiler {
    fn compile_assign(
        &mut self,
        lhs: &Token<Expr>,
        op: Operator,
        rhs: &Token<Expr>,
        deref: bool,
    ) -> Result<()>;

    fn compile_let(&mut self, lt: &Let) -> Result<()>;
}

impl<'a> AssignmentCompiler for FunctionCompiler<'a> {
    fn compile_assign(
        &mut self,
        lhs: &Token<Expr>,
        op: Operator,
        rhs: &Token<Expr>,
        deref: bool,
    ) -> Result<()> {
        if deref {
            let pointer = self.compile_expr(lhs, None)?;
            let element = match &pointer.ty {
                ComplexType::Basic(BasicType::Object(obj)) => {
                    if obj.name == "core::mem::Pointer" {
                        obj.generic_args[0].clone()
                    } else {
                        return Err(compiler_error!(
                            self,
                            "Cannot dereference non-reference type `{}`",
                            pointer.ty.to_string()
                        ));
                    }
                }
                _ => {
                    return Err(compiler_error!(
                        self,
                        "Cannot dereference non-reference type `{}`",
                        pointer.ty.to_string()
                    ))
                }
            };
            let rhs = self.compile_expr(rhs, None)?;

            let addr_ptr = self.emit(Insn::GetElementPtr(
                pointer.val,
                pointer.ty.as_llvm_type(&self.cpl),
                1,
            )); // pointer to the `address` field in the Pointer<T> struct
            let addr_int = self.emit(Insn::Load(
                addr_ptr,
                BasicType::USize.as_llvm_type(&self.cpl),
            )); // this Load loads the address stored in the Pointer<T> struct
            let addr_ptr = self.emit(Insn::IntToPtr(
                addr_int,
                element.clone().to_reference().as_llvm_type(&self.cpl),
            ));
            self.emit(Insn::Store(rhs.val, addr_ptr));

            return Ok(());
        }
        match &lhs.token {
            Expr::Member(member_expr) => {
                let previous_members = &member_expr.members[0..member_expr.members.len() - 1];
                let previous_result = if previous_members.len() == 1 {
                    self.compile_expr(&member_expr.members[0].name, None)?
                } else if previous_members.len() > 0 {
                    self.compile_expr(
                        &Token {
                            loc: lhs.loc.clone(),
                            token: Expr::Member(MemberExpr {
                                members: previous_members.to_vec(),
                            }),
                        },
                        None,
                    )?
                } else {
                    return self.compile_assign(&member_expr.members[0].name, op, rhs, deref);
                };

                let last_member = member_expr.members.last().unwrap();
                self.loc(&last_member.name.loc);

                match &previous_result.ty {
                    ComplexType::Basic(BasicType::Object(ident)) => {
                        let class_impl = self
                            .cpl
                            .type_provider
                            .get_class_by_name(&ident)
                            .ok_or_else(|| {
                                compiler_error!(self, "No such type `{}`", ident.to_string())
                            })?;
                        match &last_member.name.token {
                            Expr::Ident(field_name) => {
                                let class_member = self.resolve_class_member_ptr(
                                    &previous_result,
                                    &class_impl,
                                    field_name,
                                )?;
                                let val = class_member.load(self)?;
                                self.try_unscope(&TypedValue {
                                    ty: class_member.get_type(),
                                    val,
                                })?;

                                let compiled_rhs =
                                    self.compile_expr(rhs, Some(&class_member.get_type()))?;

                                self.loc(&rhs.loc);
                                class_member.store(op, self, compiled_rhs)?;
                            }
                            x => unimplemented!("{:?}", x),
                        }
                    }
                    ComplexType::Array(element_type) => {
                        let index = self.compile_expr(
                            &last_member.name,
                            Some(&BasicType::USize.to_complex()),
                        )?;
                        self.assert_assignable_to(&index.ty, &BasicType::USize.to_complex())?;
                        let mut compiled_rhs = self.compile_expr(rhs, Some(&element_type))?;

                        if op == Operator::Add {
                            let element = self.load_array_element(&previous_result, &index)?;
                            let total = self.emit(Insn::IAdd(element.val, compiled_rhs.val));
                            compiled_rhs = TypedValue::new(element.ty, total);
                        }

                        self.loc(&rhs.loc);
                        self.store_array_element(&previous_result, &compiled_rhs, &index, false)?;
                    }
                    _ => {
                        return Err(compiler_error!(
                            self,
                            "[ER7] The `.` operator is forbidden on type `{}`",
                            previous_result.ty.to_string()
                        ))
                    }
                }
            }
            Expr::StaticFieldReference(ident) => match self.resolve_static_field_reference(ident) {
                Ok(static_field) => {
                    self.try_unscope(&static_field)?;
                    let compiled_rhs = self.compile_expr(rhs, Some(&static_field.ty))?;

                    self.store(op, compiled_rhs, &static_field)?;
                }
                Err(e) => {
                    if ident.0.len() == 1 {
                        let local_var = self.resolve_ident(&ident.0[0]).map_err(|_| e)?;
                        if local_var.source == LocalVarSource::Scalar {
                            return Err(compiler_error!(self, "Cannot assign to scalar values"));
                        }
                        self.try_unscope(&local_var.value)?;
                        let compiled_rhs = self.compile_expr(rhs, Some(&local_var.value.ty))?;

                        self.store(op, compiled_rhs, &local_var.value)?;

                        return Ok(());
                    }
                    return Err(e);
                }
            },
            x => unimplemented!("{:?}", x),
        }

        Ok(())
    }

    fn compile_let(&mut self, lt: &Let) -> Result<()> {
        if self.resolve_ident(&lt.name).is_ok() {
            return Err(compiler_error!(
                &self,
                "Duplicate identifier: {}",
                lt.name.token.0
            ));
        }

        self.state.get_current_block_mut().locals.push(LocalVar {
            name: lt.name.token.0.clone(),
            source: LocalVarSource::Pointer,
            value: self.cpl.context.const_unknown(),
        });

        let (initial_ref, var_type) = match (&lt.var_type, &lt.initial_value) {
            (Some(var_type), Some(initial_value)) => {
                self.loc(&var_type.loc);
                let var_type = self.resolve_type(&var_type.complex)?;
                let initial_ref = self.compile_expr(initial_value, Some(&var_type))?;

                self.loc(&initial_value.loc);
                self.assert_assignable_to(&initial_ref.ty, &var_type)?;

                (initial_ref, var_type)
            }
            (Some(_var_type), None) => {
                panic!("Variables without an initial value are WIP: {:#?}", lt);
            }
            (None, Some(initial_value)) => {
                self.loc(&initial_value.loc);
                let initial_ref = self.compile_expr(initial_value, None)?;
                let var_type = initial_ref.ty.clone();
                if var_type == BasicType::Void.to_complex() {
                    return Err(compiler_error!(self, "Ilegal variable type `void`"));
                }
                (initial_ref, var_type)
            }
            (None, None) => unreachable!(),
        };

        let typed_local_var =
            if self.try_scope(&TypedValue::new(var_type.clone(), initial_ref.val))? {
                TypedValue {
                    ty: var_type,
                    val: initial_ref.val,
                }
            } else {
                let var_ref = self.emit(Insn::Alloca(var_type.as_llvm_type(self.cpl)));
                let typed_local_var = TypedValue {
                    ty: var_type,
                    val: var_ref,
                };
                self.copy(&initial_ref, &typed_local_var)?;
                typed_local_var
            };

        self.state
            .get_current_block_mut()
            .locals
            .last_mut()
            .unwrap()
            .value = typed_local_var;

        Ok(())
    }
}
